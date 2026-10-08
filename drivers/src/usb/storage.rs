//! `usb-storage`: Bulk-Only disks. Each stick's transport is a request engine
//! queue with two slots, so one command is on the wire and one waits; the
//! drain moves a command along, the USB thread recovers the device, and each
//! LUN registers as a disk named `sd` and a letter.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_fs::blockdev::BlockDevice;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock, WaitAbort, WaitQueue};
use slopos_ostd::{
    KArc, KBox, klog_info, lock_class, write_array_field, write_field, write_init_field,
};
use slopos_usb_core::bus::Path;
use slopos_usb_core::device::descriptor::{Configuration, TransferType};
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::storage::bot::{CBW_LEN, MAX_LUN};
use slopos_usb_core::storage::scsi::{self, Capacity, CapacityAnswer, Command, Inquiry, sense_key};
use slopos_usb_core::storage::transport::{
    Buffer, CBW_AT, CSW_AT, Counts, Done, Interface, Outcome, Pipe, PipeCommand, QUEUE, SCRATCH_AT,
    SCRATCH_LEN, SENSE_AT, Stepped, TAG_MS, Target, Transport, Wire,
};
use slopos_usb_core::storage::{CLASS, PROTOCOL_BULK_ONLY, SUBCLASS_SCSI};
use slopos_usb_core::xhci::ring::{CommandResult, SubmitError, Ticket};
use slopos_usb_core::xhci::transfer::{MAX_TD_PAGES, PushError, Transfer, TransferResult};

use super::bus::{BoundUsbDevice, UsbFunction, UsbMatch};
use super::xhci::device::{STREAM_IN, STREAM_OUT, Stream, StreamSink, Translator};
use crate::block::engine::{BlkError, Engine, Op, QueueOps, Request, RequestPages};
use crate::block::{self, DiskName, EngineDisk};
use crate::driver_core::bus::{ProbeError, ProbeOutcome, Removal};

const MAX_STICKS: usize = 8;
const MAX_LUNS: usize = 8;
/// Linux's default for USB mass storage, which it keeps to work with as many
/// devices as possible.
const MAX_TRANSFER: usize = 120 * 1024;
/// One command on the wire, one queued behind it.
const SLOTS: usize = 2;
const READY_PAUSE_MS: u64 = 100;
/// Sends of a probe command the device answered but did not complete.
const PROBE_TRIES: usize = 3;
/// What a shutdown gives the engines to finish, and each flush.
const DRAIN_MS: u32 = 2000;
const FLUSH_MS: u32 = 5000;

/// The device's side of the transport: its pipes, until it leaves.
#[derive(slopos_ostd::SlotFields)]
struct Link {
    stream: Option<KArc<Stream>>,
    translator: Option<Translator>,
    held: [[u64; MAX_TD_PAGES]; QUEUE],
    held_len: [u8; QUEUE],
}

impl Link {
    fn init(stream: KArc<Stream>, translator: Option<Translator>) -> impl Init<Self, AllocError> {
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, stream, Some(stream));
                write_field!(slot, translator, translator);
                write_array_field!(slot, held, QUEUE, |_| [0u64; MAX_TD_PAGES]);
                write_field!(slot, held_len, [0; QUEUE]);
                Ok(slot.finish())
            },
        )
    }

    fn pipe(pipe: Pipe) -> usize {
        match pipe {
            Pipe::In => STREAM_IN,
            Pipe::Out => STREAM_OUT,
        }
    }
}

impl Wire for Link {
    fn now_ms(&mut self) -> u64 {
        slopos_kernel_services::clock::uptime_ms()
    }

    fn gone(&mut self) -> bool {
        self.stream.as_ref().is_none_or(|s| s.is_gone())
    }

    fn hold(&mut self, slot: u8, pages: &[u64]) {
        let slot = usize::from(slot) % QUEUE;
        let count = pages.len().min(MAX_TD_PAGES);
        self.held[slot][..count].copy_from_slice(&pages[..count]);
        self.held_len[slot] = count as u8;
    }

    fn write_wrapper(&mut self, cbw: &[u8; CBW_LEN]) {
        if let Some(stream) = &self.stream {
            stream.write_wire(CBW_AT, cbw);
        }
    }

    fn read_wire(&mut self, at: usize, out: &mut [u8]) {
        match &self.stream {
            Some(stream) => stream.read_wire(at, out),
            None => out.fill(0),
        }
    }

    fn bulk(&mut self, pipe: Pipe, buffer: Buffer, length: u32) -> Result<Transfer, PushError> {
        let stream = self.stream.as_ref().ok_or(PushError::Halted)?;
        let wire = |offset: usize| [stream.wire_phys() + offset as u64];
        let pipe = Self::pipe(pipe);
        match buffer {
            Buffer::Wrapper => stream.push(pipe, &wire(CBW_AT), length),
            Buffer::Status => stream.push(pipe, &wire(CSW_AT), length),
            Buffer::Scratch => stream.push(pipe, &wire(SCRATCH_AT), length),
            Buffer::Sense => stream.push(pipe, &wire(SENSE_AT), length),
            Buffer::Held(slot) => {
                let slot = usize::from(slot) % QUEUE;
                let count = usize::from(self.held_len[slot]);
                stream.push(pipe, &self.held[slot][..count], length)
            }
        }
    }

    fn bulk_result(&mut self, pipe: Pipe, transfer: Transfer) -> Option<TransferResult> {
        self.stream.as_ref()?.result(Self::pipe(pipe), transfer)
    }

    fn abandon(&mut self, pipe: Pipe, transfer: Transfer) {
        if let Some(stream) = &self.stream {
            stream.abandon(Self::pipe(pipe), transfer);
        }
    }

    fn control(&mut self, target: Target, setup: Setup) -> Result<Transfer, PushError> {
        match target {
            Target::Device => self
                .stream
                .as_ref()
                .ok_or(PushError::Halted)?
                .control(setup),
            Target::TranslatorHub => self
                .translator
                .as_ref()
                .ok_or(PushError::Halted)?
                .control(setup),
        }
    }

    fn control_result(&mut self, target: Target, transfer: Transfer) -> Option<TransferResult> {
        match target {
            Target::Device => self.stream.as_ref()?.control_result(transfer),
            Target::TranslatorHub => self.translator.as_ref()?.control_result(transfer),
        }
    }

    fn abandon_control(&mut self, target: Target, transfer: Transfer) {
        match target {
            Target::Device => {
                if let Some(stream) = &self.stream {
                    stream.abandon_control(transfer);
                }
            }
            Target::TranslatorHub => {
                if let Some(translator) = &self.translator {
                    translator.abandon_control(transfer);
                }
            }
        }
    }

    fn command(&mut self, command: PipeCommand) -> Result<Ticket, SubmitError> {
        let stream = self.stream.as_ref().ok_or(SubmitError::Dead)?;
        match command {
            PipeCommand::Reset(pipe) => stream.reset_endpoint(Self::pipe(pipe)),
            PipeCommand::Stop(pipe) => stream.stop_endpoint(Self::pipe(pipe)),
            PipeCommand::Reconfigure(mask) => {
                let mut pipes = 0;
                for pipe in [Pipe::In, Pipe::Out] {
                    if mask & pipe.bit() != 0 {
                        pipes |= 1 << Self::pipe(pipe);
                    }
                }
                stream.readd(pipes)
            }
        }
    }

    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult> {
        self.stream.as_ref()?.command_result(ticket)
    }

    fn abandon_command(&mut self, ticket: Ticket) {
        if let Some(stream) = &self.stream {
            stream.abandon_command(ticket);
        }
    }

    fn stuck(&mut self) {
        if let Some(stream) = &self.stream {
            stream.stuck();
        }
    }

    fn recovered(&mut self, pipe: Pipe) {
        if let Some(stream) = &self.stream {
            stream.recovered(Self::pipe(pipe));
        }
    }

    fn escalate(&mut self) {
        if let Some(stream) = &self.stream {
            stream.escalate();
        }
    }
}

#[derive(slopos_ostd::SlotFields)]
struct Inner {
    transport: Transport,
    link: Link,
    /// The transport's counts the log has reported.
    logged: Counts,
}

/// The transport and the link it drives, behind the lock submission, the
/// drain and the USB thread share. Built in place: the held page lists are
/// too large for a stack.
#[derive(slopos_ostd::SlotFields)]
struct Shared {
    inner: SpinLock<Inner>,
}

impl Shared {
    fn new(
        interface: Interface,
        stream: KArc<Stream>,
        translator: Option<Translator>,
    ) -> Result<KArc<Self>, AllocError> {
        KArc::try_init(init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let inner = init_struct_with(
                    move |inner: SlotPtr<Inner>| -> Result<Initialised<Inner>, AllocError> {
                        write_field!(inner, transport, Transport::new(interface));
                        write_init_field!(inner, link, Link::init(stream, translator))?;
                        write_field!(inner, logged, Counts::default());
                        Ok(inner.finish())
                    },
                );
                write_init_field!(
                    slot,
                    inner,
                    SpinLock::init_with(
                        lock_class!("usb-storage.transport", LOCK_LEVEL_RESOURCE),
                        inner
                    )
                )?;
                Ok(slot.finish())
            },
        ))
    }

    fn completed(&self) -> Stepped {
        let mut inner = self.inner.lock();
        let Inner {
            transport, link, ..
        } = &mut *inner;
        transport.completed(link)
    }

    /// The step, when it next needs one, and what recovery did since the
    /// log last said.
    fn serve(&self) -> (Stepped, Option<u64>, Counts, Counts) {
        let mut inner = self.inner.lock();
        let Inner {
            transport,
            link,
            logged,
        } = &mut *inner;
        let (stepped, next) = transport.serve(link);
        let before = *logged;
        *logged = transport.counts;
        (stepped, next, before, transport.counts)
    }

    fn submit_direct(&self, lun: u8, command: Command) -> Option<u16> {
        let mut inner = self.inner.lock();
        let Inner {
            transport, link, ..
        } = &mut *inner;
        transport.submit(link, lun, command, None, true)
    }

    /// Sends `command` to `lun` from the bind thread and waits for it; its
    /// data, at most [`SCRATCH_LEN`] bytes, lands in `data`. `None` when it
    /// never finished.
    fn run(&self, lun: u8, command: Command, data: &mut [u8]) -> Option<Done> {
        let tag = self.submit_direct(lun, command)?;
        let waited =
            super::TRANSFERS.wait_event_timeout_until(|| self.transport(|t| t.take(tag)), TAG_MS);
        let mut done = match waited {
            Ok(done) => done,
            Err(
                WaitAbort::Killed
                | WaitAbort::Interrupted
                | WaitAbort::Timeout
                | WaitAbort::NoRuntime,
            ) => {
                self.transport(|t| t.retire(tag));
                return None;
            }
        };
        let length = (done.moved as usize).min(data.len()).min(SCRATCH_LEN);
        self.inner
            .lock()
            .link
            .read_wire(SCRATCH_AT, &mut data[..length]);
        done.moved = length as u32;
        Some(done)
    }

    fn transport<R>(&self, f: impl FnOnce(&mut Transport) -> R) -> R {
        f(&mut self.inner.lock().transport)
    }

    /// Lets the device go, abandoning what its pipes and EP0 still hold:
    /// nothing of it is touched again. The pipes are the caller's to drop
    /// with this lock released.
    fn release(&self) -> (Option<KArc<Stream>>, Option<Translator>) {
        let mut inner = self.inner.lock();
        let Inner {
            transport, link, ..
        } = &mut *inner;
        transport.leave(link);
        (link.stream.take(), link.translator.take())
    }
}

/// The engine's queue: each request becomes a READ, WRITE or SYNCHRONIZE
/// CACHE on the namespace's LUN.
struct Queue(KArc<Shared>);

impl QueueOps for Queue {
    fn submit(&mut self, req: &Request, pages: &RequestPages) -> Result<u16, BlkError> {
        let shift = req.ns.block_shift;
        let command = match req.op {
            Op::Flush => Command::synchronize_cache(),
            op => Command::transfer(
                op == Op::Write,
                req.offset >> shift,
                (req.len >> shift) as u32,
                1 << shift,
            )
            .ok_or(BlkError::BadRequest)?,
        };
        let span = pages.span(req.len);
        let mut phys = [0u64; MAX_TD_PAGES];
        for (out, page) in phys.iter_mut().zip(span) {
            *out = page.phys_u64();
        }
        let held = (req.op != Op::Flush).then_some(&phys[..span.len().min(MAX_TD_PAGES)]);
        let mut inner = self.0.inner.lock();
        let Inner {
            transport, link, ..
        } = &mut *inner;
        transport
            .submit(link, req.ns.nsid as u8, command, held, false)
            .ok_or(BlkError::Busy)
    }

    fn pop(&mut self) -> Option<(u16, u32)> {
        let (tag, outcome) = self.0.transport(Transport::pop)?;
        Some((tag, outcome.code()))
    }

    fn retire(&mut self, tag: u16) {
        self.0.transport(|t| t.retire(tag));
    }

    fn outcome(&self, _pages: &RequestPages, status: u32) -> Result<(), BlkError> {
        match Outcome::from_code(status) {
            Some(Outcome::Ok) => Ok(()),
            Some(Outcome::Retry) => Err(BlkError::DeviceFault { retry: true }),
            Some(Outcome::Fail | Outcome::Gone) | None => {
                Err(BlkError::DeviceFault { retry: false })
            }
        }
    }
}

/// Moves the command on wherever the event ring is drained, and wakes its
/// waiters.
struct Waker {
    shared: KArc<Shared>,
    engine: KArc<Engine>,
}

impl StreamSink for Waker {
    fn completed(&self) {
        let stepped = self.shared.completed();
        finished(&stepped, &self.engine);
    }
}

fn finished(stepped: &Stepped, engine: &Engine) {
    if stepped.finished {
        engine.handle_irq();
        super::TRANSFERS.wake_all();
    }
    if stepped.service {
        super::wake();
    }
}

#[derive(slopos_ostd::SlotFields)]
struct Storage {
    controller: u8,
    path: Path,
    shared: KArc<Shared>,
    engine: KArc<Engine>,
    disks: SpinLock<[Option<DiskName>; MAX_LUNS]>,
    retired: AtomicBool,
}

impl Storage {
    /// Leaves the registry, fails what the device still owes, and takes its
    /// disks out of `/dev`; a mount keeps its disk until it lets go.
    fn retire(&self) {
        if self.retired.swap(true, Ordering::AcqRel) {
            return;
        }
        unregister(self);
        self.engine.stop();
        let released = self.shared.release();
        self.engine.handle_irq();
        super::TRANSFERS.wake_all();
        drop(released);
        for lun in 0..MAX_LUNS {
            let taken = self.disks.lock()[lun].take();
            if let Some(name) = taken {
                block::unregister_disk(name);
                klog_info!("USB: {}-{} {} removed", self.controller, self.path, name);
            }
        }
    }

    fn serve(&self) -> Option<u64> {
        let (stepped, next, before, now) = self.shared.serve();
        finished(&stepped, &self.engine);
        self.log_recovery(before, now);
        next
    }

    #[inline(never)]
    fn log_recovery(&self, before: Counts, now: Counts) {
        let (c, path) = (self.controller, self.path);
        if now.clears != before.clears {
            klog_info!("USB: {}-{} a halted pipe cleared", c, path);
        }
        if now.resets != before.resets {
            klog_info!("USB: {}-{} Bulk-Only reset recovery", c, path);
        }
        if now.escalations != before.escalations {
            klog_info!("USB: {}-{} recovery failed; the port is reset", c, path);
        }
    }
}

/// A queue nothing wakes: TEST UNIT READY's retries pause on it.
static PAUSE: WaitQueue = WaitQueue::new(lock_class!("usb-storage.pause", LOCK_LEVEL_RESOURCE));

static STICKS: SpinLock<[Option<KArc<Storage>>; MAX_STICKS]> = SpinLock::new(
    [const { None }; MAX_STICKS],
    lock_class!("usb-storage.STICKS", LOCK_LEVEL_RESOURCE),
);

fn register(storage: &KArc<Storage>) -> bool {
    let mut sticks = STICKS.lock();
    let Some(free) = sticks.iter_mut().find(|s| s.is_none()) else {
        return false;
    };
    *free = Some(KArc::clone(storage));
    true
}

fn unregister(storage: &Storage) {
    let taken = {
        let mut sticks = STICKS.lock();
        sticks
            .iter_mut()
            .find(|s| s.as_deref().is_some_and(|s| core::ptr::eq(s, storage)))
            .and_then(Option::take)
    };
    drop(taken);
}

fn sticks(controller: Option<u8>) -> [Option<KArc<Storage>>; MAX_STICKS] {
    let mut out = [const { None }; MAX_STICKS];
    for (stick, out) in STICKS
        .lock()
        .iter()
        .flatten()
        .filter(|s| controller.is_none_or(|c| c == s.controller))
        .zip(out.iter_mut())
    {
        *out = Some(KArc::clone(stick));
    }
    out
}

/// The USB thread's pass over every stick: deadlines and recoveries. When it
/// next needs one.
pub(super) fn serve() -> Option<u64> {
    sticks(None)
        .iter()
        .flatten()
        .filter_map(|s| s.serve())
        .min()
}

/// A controller's shutdown, polled, its threads stopped: the engines are
/// stopped and drained, then each LUN with a cache whose pipes came to rest
/// is flushed below the engine, with no recovery.
pub(crate) fn shutdown(controller: u8, drain: &dyn Fn()) {
    let sticks = sticks(Some(controller));
    for stick in sticks.iter().flatten() {
        stick.engine.stop();
    }
    let idle = |s: &Storage| s.engine.is_idle() && s.shared.transport(|t| t.is_idle());
    crate::hpet::spin_until(
        &mut || {
            drain();
            sticks.iter().flatten().all(|s| idle(s))
        },
        DRAIN_MS,
    );
    for stick in sticks.iter().flatten() {
        if !idle(stick) || stick.shared.transport(|t| t.is_broken()) {
            klog_info!(
                "USB: {}-{} left mid-command; not flushed",
                stick.controller,
                stick.path
            );
            continue;
        }
        flush(stick, drain);
    }
}

#[inline(never)]
fn flush(stick: &Storage, drain: &dyn Fn()) {
    stick.shared.transport(Transport::forbid_recovery);
    for lun in 0..MAX_LUNS as u8 {
        let served = stick.disks.lock()[usize::from(lun)].is_some();
        if !served || !stick.shared.transport(|t| t.has_cache(lun)) {
            continue;
        }
        let Some(tag) = stick
            .shared
            .submit_direct(lun, Command::synchronize_cache())
        else {
            continue;
        };
        let mut done = None;
        crate::hpet::spin_until(
            &mut || {
                drain();
                stick.serve();
                done = stick.shared.transport(|t| t.take(tag));
                done.is_some()
            },
            FLUSH_MS,
        );
        if done.is_none_or(|d| d.outcome != Outcome::Ok) {
            klog_info!(
                "USB: {}-{} LUN {} not flushed",
                stick.controller,
                stick.path,
                lun
            );
        }
        if done.is_none() {
            stick.shared.transport(|t| t.retire(tag));
            return;
        }
    }
}

struct Unplugged(KArc<Storage>);

impl Removal for Unplugged {
    fn remove(&self) {
        self.0.retire();
    }
}

fn bulk_pair(config: &Configuration<'_>, interface: u8) -> Option<(u8, u8)> {
    let bulk = |input: bool| {
        config
            .endpoints(interface, 0)
            .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == input)
            .map(|e| e.address)
    };
    Some((bulk(true)?, bulk(false)?))
}

/// A device that stalls the request has one LUN (Bulk-Only §3.2).
fn max_lun(bound: &mut BoundUsbDevice<'_>, interface: u8) -> u8 {
    let Ok(control) = bound.control() else {
        return 0;
    };
    let mut answer = [0u8; 1];
    match control.read(Setup::get_max_lun(interface), &mut answer) {
        Ok(1) if answer[0] <= MAX_LUN => answer[0],
        _ => 0,
    }
}

fn probe(bound: &mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError> {
    let info = *bound.info();
    let interface = info.first_interface;
    let Some((bulk_in, bulk_out)) = bound
        .descriptors(|config| bulk_pair(config, interface))
        .flatten()
    else {
        return Ok(ProbeOutcome::Declined);
    };
    let luns = max_lun(bound, interface).saturating_add(1);
    let stream = bound
        .stream(bulk_in, bulk_out)
        .map_err(|_| ProbeError::DeviceFault)?;
    let translator = stream.translator();
    let translated = translator.is_some().then(|| stream.address());
    let interface = Interface {
        interface,
        bulk_in,
        bulk_out,
        translated,
    };
    let shared = Shared::new(interface, KArc::clone(&stream), translator)
        .map_err(|_| ProbeError::OutOfMemory)?;
    let attached = start_engine(&shared, &stream).and_then(|engine| attach(&info, &shared, engine));
    let storage = match attached {
        Ok(storage) => storage,
        Err(e) => {
            drop(shared.release());
            return Err(e);
        }
    };
    // Before any LUN is probed: the USB thread recovers only registered
    // sticks, and a probe command may need it.
    if !register(&storage) {
        storage.retire();
        klog_info!(
            "USB: {}-{} declined: no room for more sticks",
            info.controller,
            info.path
        );
        return Ok(ProbeOutcome::Declined);
    }
    if bound.on_remove(Unplugged(KArc::clone(&storage))).is_err() {
        storage.retire();
        return Err(ProbeError::OutOfMemory);
    }
    serve_luns(&storage, luns);
    if storage.disks.lock().iter().all(Option::is_none) {
        storage.retire();
        return Ok(ProbeOutcome::Declined);
    }
    Ok(ProbeOutcome::Bound)
}

#[inline(never)]
fn start_engine(shared: &KArc<Shared>, stream: &Stream) -> Result<KArc<Engine>, ProbeError> {
    let engine = KArc::try_init(Engine::init(
        "usb-storage",
        SLOTS,
        MAX_TRANSFER,
        2 * TAG_MS,
        TAG_MS,
    ))
    .map_err(|_| ProbeError::OutOfMemory)?;
    if !engine.prime() {
        return Err(ProbeError::OutOfMemory);
    }
    let queue: KBox<dyn QueueOps> =
        KBox::try_new(Queue(KArc::clone(shared))).map_err(|_| ProbeError::OutOfMemory)?;
    engine.start(queue);
    let waker = KArc::try_new(Waker {
        shared: KArc::clone(shared),
        engine: KArc::clone(&engine),
    })
    .map_err(|_| ProbeError::OutOfMemory)?;
    stream.attach(waker).map_err(|_| ProbeError::OutOfMemory)?;
    Ok(engine)
}

#[inline(never)]
fn attach(
    info: &UsbFunction,
    shared: &KArc<Shared>,
    engine: KArc<Engine>,
) -> Result<KArc<Storage>, ProbeError> {
    let (controller, path) = (info.controller, info.path);
    let shared = KArc::clone(shared);
    KArc::try_init(init_struct_with(
        move |slot: SlotPtr<Storage>| -> Result<Initialised<Storage>, AllocError> {
            write_field!(slot, controller, controller);
            write_field!(slot, path, path);
            write_field!(slot, shared, shared);
            write_field!(slot, engine, engine);
            write_field!(
                slot,
                disks,
                SpinLock::new(
                    [None; MAX_LUNS],
                    lock_class!("usb-storage.disks", LOCK_LEVEL_RESOURCE)
                )
            );
            write_field!(slot, retired, AtomicBool::new(false));
            Ok(slot.finish())
        },
    ))
    .map_err(|_| ProbeError::OutOfMemory)
}

#[inline(never)]
fn serve_luns(storage: &Storage, luns: u8) {
    for lun in 0..luns.min(MAX_LUNS as u8) {
        match probe_lun(storage, lun) {
            Lun::Disk(disk) => {
                if let Some(name) = register_lun(storage, lun, disk) {
                    storage.disks.lock()[usize::from(lun)] = Some(name);
                }
            }
            Lun::Declined => {}
            Lun::Silent => return,
        }
    }
    if luns as usize > MAX_LUNS {
        klog_info!(
            "USB: {}-{} LUNs past {} left unserved",
            storage.controller,
            storage.path,
            MAX_LUNS
        );
    }
}

enum Lun {
    Disk(EngineDisk),
    Declined,
    /// INQUIRY went unanswered: no LUN past this one is asked, so a stick
    /// that hangs holds the bind thread for one command's recovery.
    Silent,
}

/// A LUN as a disk, once it says what it is, is ready and has a capacity.
#[inline(never)]
fn probe_lun(storage: &Storage, lun: u8) -> Lun {
    let shared = &storage.shared;
    let mut data = [0u8; SCRATCH_LEN];
    let say = |why: &str| {
        klog_info!(
            "USB: {}-{} LUN {} not served: {}",
            storage.controller,
            storage.path,
            lun,
            why
        );
    };
    let decline = |why: &str| {
        say(why);
        Lun::Declined
    };
    let Some(inquiry) = (0..PROBE_TRIES)
        .map_while(|_| shared.run(lun, Command::inquiry(), &mut data))
        .find(|done| done.outcome != Outcome::Retry)
    else {
        say("no answer to INQUIRY");
        return Lun::Silent;
    };
    match (inquiry.outcome, inquiry.sense) {
        (Outcome::Ok, _) => {}
        (Outcome::Fail, Some(_)) => return decline("INQUIRY refused"),
        _ => {
            say("no answer to INQUIRY");
            return Lun::Silent;
        }
    }
    if !Inquiry::parse(&data[..inquiry.moved as usize]).is_some_and(|i| i.is_disk()) {
        return decline("not a disk");
    }
    if let Err(why) = ready(shared, lun) {
        return decline(why);
    }
    let Some(capacity) = capacity(shared, lun) else {
        return decline("no capacity");
    };
    let Some(bytes) = capacity.bytes() else {
        return decline("a block size not served");
    };
    let mode = shared.run(lun, Command::mode_sense(), &mut data);
    let protected = mode
        .filter(|m| m.outcome == Outcome::Ok)
        .and_then(|m| scsi::write_protected(&data[..m.moved as usize]))
        .unwrap_or(false);
    let disk = EngineDisk::new(
        KArc::clone(&storage.engine),
        u32::from(lun),
        capacity.block_size,
        bytes,
        true,
    );
    Lun::Disk(if protected { disk.protected() } else { disk })
}

/// TEST UNIT READY until it passes, or for `usb.settle_ms`.
fn ready(shared: &Shared, lun: u8) -> Result<(), &'static str> {
    let start = slopos_kernel_services::clock::uptime_ms();
    let budget = u64::from(super::settle_ms());
    loop {
        let done = shared
            .run(lun, Command::test_unit_ready(), &mut [])
            .ok_or("it did not answer")?;
        match (done.outcome, done.sense) {
            (Outcome::Ok, _) => return Ok(()),
            (Outcome::Retry, Some(s))
                if s.key == sense_key::NOT_READY && s.asc == scsi::asc::MEDIUM_NOT_PRESENT =>
            {
                return Err("no medium");
            }
            (Outcome::Retry, _) => {}
            _ => return Err("TEST UNIT READY failed"),
        }
        if slopos_kernel_services::clock::uptime_ms().saturating_sub(start) >= budget {
            return Err("not ready");
        }
        match PAUSE.wait_event_timeout(|| false, READY_PAUSE_MS) {
            Err(WaitAbort::Timeout) => {}
            Ok(()) | Err(WaitAbort::Killed | WaitAbort::Interrupted | WaitAbort::NoRuntime) => {
                return Err("its wait was cut short");
            }
        }
    }
}

fn capacity(shared: &Shared, lun: u8) -> Option<Capacity> {
    let mut data = [0u8; SCRATCH_LEN];
    for _ in 0..PROBE_TRIES {
        let done = shared.run(lun, Command::read_capacity_10(), &mut data)?;
        match done.outcome {
            Outcome::Ok => {}
            Outcome::Retry => continue,
            _ => return None,
        }
        match Capacity::parse_10(&data[..done.moved as usize])? {
            CapacityAnswer::Known(capacity) => return Some(capacity),
            CapacityAnswer::TooLarge => {
                let done = shared.run(lun, Command::read_capacity_16(), &mut data)?;
                if done.outcome != Outcome::Ok {
                    return None;
                }
                return Capacity::parse_16(&data[..done.moved as usize]);
            }
        }
    }
    None
}

#[inline(never)]
fn register_lun(storage: &Storage, lun: u8, disk: EngineDisk) -> Option<DiskName> {
    let protected = disk.write_protected();
    let (bytes, block) = (disk.capacity(), disk.logical_block_size());
    let disk = KArc::try_new(disk).ok()?;
    let Some(name) = block::register_usb_disk(disk) else {
        klog_info!(
            "USB: {}-{} LUN {} declined: no room for more disks",
            storage.controller,
            storage.path,
            lun
        );
        return None;
    };
    klog_info!(
        "USB: {}-{} LUN {} is {}, {} MB in {}-byte blocks{}",
        storage.controller,
        storage.path,
        lun,
        name,
        bytes / (1024 * 1024),
        block,
        if protected { ", write-protected" } else { "" }
    );
    Some(name)
}

crate::usb_driver! {
    pub static USB_STORAGE = {
        name: "usb-storage",
        match_table: &[UsbMatch::Class {
            class: CLASS,
            subclass: Some(SUBCLASS_SCSI),
            protocol: Some(PROTOCOL_BULK_ONLY),
        }],
        probe: probe,
    };
}

#[cfg(feature = "test-hooks")]
pub fn has_disk(name: &[u8]) -> bool {
    sticks(None).iter().flatten().any(|s| {
        let disks = s.disks.lock();
        disks.iter().flatten().any(|d| d.as_bytes() == name)
    })
}
