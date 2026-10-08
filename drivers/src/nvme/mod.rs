//! NVMe over PCIe: an admin queue, one interrupt-driven I/O queue pair that
//! every namespace shares through the block layer's request engine, a polled
//! pair kept for the panic path, and the host memory buffer a DRAM-less drive
//! asks for. Register and structure layouts come from `slopos-nvme-core`.

mod admin;
mod hmb;
mod io;
mod panic;
mod ring;

pub use panic::{PanicQueue, PanicSession};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_nvme_core::command::{Command, SQE_BYTES, cns};
use slopos_nvme_core::completion::CQE_BYTES;
use slopos_nvme_core::identify::{self, ControllerInfo, NamespaceInfo, trim_ascii};
use slopos_nvme_core::regs::{self, CC_EN, Cap, Csts, PAGE_SIZE};
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, OnceLock};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class, write_field, write_init_field};

use crate::block::engine::{self, Engine};
use crate::block::{self, DiskName, EngineDisk};
use crate::driver_core::msi::{self as core_msi, IrqMechanism};
use crate::driver_core::shutdown::{self, DeviceShutdown};
use crate::pci::{
    BoundDevice, PciMatch, PciProbeError, ProbeOutcome, disable_bus_master, enable_bus_master,
    enable_memory_space,
};
use admin::{AdminError, AdminQueue};
use hmb::HostMemoryBuffer;
use io::IoQueue;
use ring::Ring;

const CLASS_MASS_STORAGE: u8 = 0x01;
const SUBCLASS_NVM: u8 = 0x08;
const PROG_IF_NVME: u8 = 0x02;

const ADMIN_QID: u16 = 0;
const IO_QID: u16 = 1;
const PANIC_QID: u16 = 2;
const ADMIN_DEPTH: u16 = 32;
const IO_DEPTH: u16 = ring::MAX_DEPTH;
const PANIC_DEPTH: u16 = 4;
const IO_SLOTS: usize = engine::MAX_SLOTS;
/// A drive busy with garbage collection or a cache flush can hold a command
/// for seconds; past this long it has lost it.
const IO_TIMEOUT_MS: u64 = 30_000;
/// MSI-X entry 0 is the admin queue's by the specification; the I/O queue's
/// is the next.
const IO_VECTOR: u16 = 1;
const MAX_CONTROLLERS: usize = 8;
/// An Active Namespace ID list holds this many IDs.
const MAX_NAMESPACES: u32 = 1024;
/// Shutdown may take RTD3E; a controller that reports nothing gets this.
const SHUTDOWN_FLOOR_MS: u32 = 5000;
const SHUTDOWN_CEILING_MS: u32 = 60_000;
/// Outstanding I/O a shutdown lets finish before it deletes the queues.
const DRAIN_MS: u32 = 1000;

#[derive(Clone, Copy)]
enum InitError {
    Unsupported(&'static str),
    Timeout(&'static str),
    Fatal,
    Absent,
    Admin(&'static str, AdminError),
    NoMemory,
}

impl core::fmt::Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InitError::Unsupported(what) => write!(f, "unsupported: {what}"),
            InitError::Timeout(step) => write!(f, "timed out waiting to {step}"),
            InitError::Fatal => f.write_str("controller fatal status"),
            InitError::Absent => f.write_str("controller gone"),
            InitError::Admin(step, e) => write!(f, "{step} failed: {e:?}"),
            InitError::NoMemory => f.write_str("out of memory"),
        }
    }
}

impl From<InitError> for PciProbeError {
    fn from(err: InitError) -> Self {
        match err {
            InitError::Unsupported(_) => PciProbeError::Unsupported,
            InitError::NoMemory => PciProbeError::OutOfMemory,
            _ => PciProbeError::DeviceFault,
        }
    }
}

#[derive(slopos_ostd::SlotFields)]
pub struct Controller {
    index: usize,
    regs: MmioRegion,
    info: ControllerInfo,
    admin: KArc<AdminQueue>,
    engine: KArc<Engine>,
    panic: Option<KBox<PanicQueue>>,
    host_memory: Mutex<Option<HostMemoryBuffer>>,
    /// The MSI-X table or MSI capability, kept for the device's life.
    _irq: IrqMechanism,
    shut_down: AtomicBool,
}

static CONTROLLERS: [OnceLock<KArc<Controller>>; MAX_CONTROLLERS] =
    [const { OnceLock::new() }; MAX_CONTROLLERS];
static CONTROLLER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The controller and namespace behind `nvme<C>n<N>`.
pub fn controller_of(disk: &[u8]) -> Option<(KArc<Controller>, u32)> {
    let rest = disk.strip_prefix(b"nvme")?;
    let n = rest.iter().position(|&b| b == b'n')?;
    let index: usize = core::str::from_utf8(&rest[..n]).ok()?.parse().ok()?;
    let nsid: u32 = core::str::from_utf8(&rest[n + 1..]).ok()?.parse().ok()?;
    let controller = CONTROLLERS.get(index)?.get()?;
    Some((KArc::clone(controller), nsid))
}

impl Controller {
    /// The polled queue pair kept for the panic path, if the controller had
    /// a queue pair to spare for it.
    pub fn panic_queue(&self) -> Option<&PanicQueue> {
        if self.shut_down.load(Ordering::Acquire) {
            return None;
        }
        self.panic.as_deref()
    }

    pub fn host_memory_bytes(&self) -> u64 {
        match self.host_memory.lock() {
            Ok(hmb) => hmb.as_ref().map_or(0, HostMemoryBuffer::bytes),
            Err(_) => 0,
        }
    }

    pub fn volatile_write_cache(&self) -> bool {
        self.info.volatile_write_cache
    }

    fn csts(&self) -> Csts {
        Csts(self.regs.read::<u32>(regs::CSTS))
    }

    /// Whether the controller reports a normal shutdown complete.
    #[cfg(feature = "test-hooks")]
    pub fn shutdown_complete(&self) -> bool {
        self.csts().shutdown_complete()
    }
}

/// Poll the controller's status until `ready` matches `want`.
#[inline(never)]
fn wait_ready(regs: &MmioRegion, want: bool, timeout_ms: u32) -> Result<(), InitError> {
    let mut failed = None;
    let reached = crate::hpet::spin_until(
        &mut || {
            let csts = Csts(regs.read::<u32>(regs::CSTS));
            if csts.absent() {
                failed = Some(InitError::Absent);
                return true;
            }
            if want && csts.fatal() {
                failed = Some(InitError::Fatal);
                return true;
            }
            csts.ready() == want
        },
        timeout_ms,
    );
    match failed {
        Some(err) => Err(err),
        None if reached => Ok(()),
        None => Err(InitError::Timeout(if want { "enable" } else { "disable" })),
    }
}

/// Bring the controller to disabled, the state it can be configured in.
#[inline(never)]
fn disable(regs: &MmioRegion, cap: Cap) -> Result<(), InitError> {
    let cc = regs.read::<u32>(regs::CC);
    if cc & CC_EN != 0 {
        // Clearing EN while RDY is still rising is undefined.
        if !Csts(regs.read::<u32>(regs::CSTS)).ready() {
            let _ = wait_ready(regs, true, cap.ready_timeout_ms());
        }
        regs.write::<u32>(regs::CC, cc & !CC_EN);
    }
    wait_ready(regs, false, cap.ready_timeout_ms())
}

#[inline(never)]
fn enable(regs: &MmioRegion) -> Result<(), InitError> {
    // Configured with EN clear, then enabled: CAP may change once CC is
    // written, and so may the timeout it reports.
    regs.write::<u32>(regs::CC, regs::cc_enabled() & !CC_EN);
    let cap = Cap(regs.read::<u64>(regs::CAP));
    let timeout = if cap.reports_ready_timeouts() {
        regs::crto_ready_timeout_ms(regs.read::<u32>(regs::CRTO))
    } else {
        cap.ready_timeout_ms()
    };
    regs.write::<u32>(regs::CC, regs::cc_enabled());
    wait_ready(regs, true, timeout)
}

fn admin_step<T>(step: &'static str, result: Result<T, AdminError>) -> Result<T, InitError> {
    result.map_err(|e| InitError::Admin(step, e))
}

/// Create I/O queue pair `qid`: its completion queue first, interrupting on
/// `vector` or polled when `None`.
#[inline(never)]
fn create_pair(
    admin: &AdminQueue,
    regs: &MmioRegion,
    cap: Cap,
    qid: u16,
    depth: u16,
    vector: Option<u16>,
) -> Result<Ring, InitError> {
    let ring = Ring::new(regs, cap, qid, depth).ok_or(InitError::NoMemory)?;
    let depth = ring.depth();
    admin_step(
        "create completion queue",
        admin.run(Command::create_io_cq(qid, depth, vector).with_prp(ring.cq_phys(), 0)),
    )?;
    admin_step(
        "create submission queue",
        admin.run(Command::create_io_sq(qid, depth, qid).with_prp(ring.sq_phys(), 0)),
    )?;
    Ok(ring)
}

/// The namespace IDs to identify: the controller's active list, or every ID
/// up to NN from a controller that keeps none — the list is NVMe 1.1's — or
/// answers with an empty one.
#[inline(never)]
fn namespace_ids(admin: &AdminQueue, info: &ControllerInfo, version: u32) -> KVec<u32> {
    let mut ids = KVec::new();
    if version >= regs::version(1, 1)
        && let Ok(list) = admin.identify(Command::identify(cns::ACTIVE_NAMESPACES, 0))
    {
        for nsid in identify::active_namespaces(&list) {
            let _ = ids.push(nsid);
        }
    }
    if ids.is_empty() {
        for nsid in 1..=info.namespaces.min(MAX_NAMESPACES) {
            let _ = ids.push(nsid);
        }
    }
    ids
}

fn nvme_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    if info.prog_if != PROG_IF_NVME {
        return Ok(ProbeOutcome::Declined);
    }
    klog_info!(
        "nvme: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );
    let bar = info.bars[0];
    if bar.base == 0 || bar.is_io != 0 || (bar.size as usize) < regs::DOORBELLS {
        klog_info!("nvme: BAR0 missing or too small");
        return Err(PciProbeError::Unsupported);
    }
    // Firmware can leave a controller running on queues in memory this
    // kernel has since reused: it masters the bus again only once disabled.
    disable_bus_master(&info);
    enable_memory_space(&info);
    let regs = bound
        .map_bar(0, 0, bar.size as usize)
        .map_err(|_| PciProbeError::OutOfMemory)?
        .clone();
    let cap = Cap(regs.read::<u64>(regs::CAP));
    if let Err(err) = disable(&regs, cap).and_then(|()| supported(&regs, cap)) {
        klog_info!("nvme: controller left unused: {}", err);
        return Err(err.into());
    }
    // Owned here, past the disable a failure ends in: the admin queue's pages
    // may be under a command that was never answered.
    let (Ok(admin_slot), Ok(engine_slot)) = (
        KArc::<OnceLock<KArc<AdminQueue>>>::try_new(OnceLock::new()),
        KArc::<OnceLock<KArc<Engine>>>::try_new(OnceLock::new()),
    ) else {
        return Err(PciProbeError::OutOfMemory);
    };
    let Ok(index) = CONTROLLER_COUNT.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        (n < MAX_CONTROLLERS).then_some(n + 1)
    }) else {
        klog_info!("nvme: no room for another controller");
        return Ok(ProbeOutcome::Declined);
    };
    enable_bus_master(&info);
    match attach(bound, &regs, cap, index, &admin_slot, &engine_slot) {
        Ok(controller) => {
            klog_info!(
                "nvme: nvme{} is {}, firmware {}, host memory {} KiB",
                controller.index,
                core::str::from_utf8(trim_ascii(&controller.info.model)).unwrap_or("?"),
                core::str::from_utf8(trim_ascii(&controller.info.firmware)).unwrap_or("?"),
                controller.host_memory_bytes() / 1024,
            );
            Ok(ProbeOutcome::Bound)
        }
        Err(err) => {
            klog_info!("nvme: controller left unused: {}", err);
            // An I/O queue freed before this was never rung, so the controller
            // never reads or writes it.
            let _ = disable(&regs, cap);
            disable_bus_master(&info);
            // Probes run one at a time, so a failed one hands its name on.
            let _ = CONTROLLER_COUNT.compare_exchange(
                index + 1,
                index,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            Err(err.into())
        }
    }
}

/// What this driver needs of a controller before touching it.
fn supported(regs: &MmioRegion, cap: Cap) -> Result<(), InitError> {
    if regs::register_span(cap, PANIC_QID + 1) > regs.size() {
        return Err(InitError::Unsupported("doorbells past BAR0"));
    }
    if !cap.supports_nvm_command_set() {
        return Err(InitError::Unsupported("no NVM command set"));
    }
    if !cap.supports_page_size() {
        return Err(InitError::Unsupported("no 4 KiB page size"));
    }
    Ok(())
}

#[inline(never)]
fn attach(
    bound: &mut BoundDevice<'_>,
    regs: &MmioRegion,
    cap: Cap,
    index: usize,
    admin_slot: &KArc<OnceLock<KArc<AdminQueue>>>,
    engine_slot: &KArc<OnceLock<KArc<Engine>>>,
) -> Result<KArc<Controller>, InitError> {
    let (irq, io_vector) = interrupts(bound, admin_slot, engine_slot)?;
    let (admin, info) = configure(regs, cap, admin_slot)?;
    let queues = Queues {
        index,
        admin,
        info,
        io_vector,
        irq,
    };
    let controller = open_queues(regs, cap, queues, engine_slot)?;
    serve(&controller, regs);
    Ok(controller)
}

/// Both vectors do both jobs: under plain MSI the one vector serves both
/// queues, and a harvest with nothing posted is one read. The answer carries
/// the I/O queue's vector.
#[inline(never)]
fn interrupts(
    bound: &mut BoundDevice<'_>,
    admin_slot: &KArc<OnceLock<KArc<AdminQueue>>>,
    engine_slot: &KArc<OnceLock<KArc<Engine>>>,
) -> Result<(IrqMechanism, u16), InitError> {
    let info = *bound.info();
    let (irq_admin, irq_engine) = (admin_slot.clone(), engine_slot.clone());
    let mut vectors = [0u8; 2];
    let irq = core_msi::setup_interrupts(bound, 2, &mut vectors, move |_entry: u8| {
        if let Some(admin) = irq_admin.get() {
            admin.handle_irq();
        }
        if let Some(engine) = irq_engine.get() {
            engine.handle_irq();
        }
    })
    .ok_or(InitError::Unsupported("neither MSI-X nor MSI"))?;
    let io_vector = match &irq {
        IrqMechanism::Msix { cap, .. } => {
            crate::msix::msix_enable(info.bus, info.device, info.function, cap);
            IO_VECTOR
        }
        IrqMechanism::Msi { .. } => 0,
    };
    Ok((irq, io_vector))
}

/// Publish the controller, ask to hear of a poweroff, register its
/// namespaces, then lend it memory.
#[inline(never)]
fn serve(controller: &KArc<Controller>, regs: &MmioRegion) {
    if let Some(slot) = CONTROLLERS.get(controller.index) {
        slot.call_once(|| KArc::clone(controller));
    }
    let hook: KArc<dyn DeviceShutdown> = controller.clone();
    if !shutdown::register(hook) {
        klog_info!(
            "nvme: nvme{} will not be told of a poweroff",
            controller.index
        );
    }
    let version = regs.read::<u32>(regs::VS);
    for nsid in namespace_ids(&controller.admin, &controller.info, version)
        .iter()
        .copied()
    {
        register_namespace(controller, nsid);
    }
    lend_host_memory(controller);
}

/// Give the disabled controller an admin queue, enable it and identify it.
#[inline(never)]
fn configure(
    regs: &MmioRegion,
    cap: Cap,
    admin_slot: &OnceLock<KArc<AdminQueue>>,
) -> Result<(KArc<AdminQueue>, ControllerInfo), InitError> {
    let admin = admin_queue(regs, cap)?;
    admin_slot.call_once(|| KArc::clone(&admin));
    enable(regs)?;
    let info = identify_controller(&admin)?;
    Ok((admin, info))
}

/// Create the I/O and panic queue pairs and start the engine on the first.
#[inline(never)]
fn open_queues(
    regs: &MmioRegion,
    cap: Cap,
    queues: Queues,
    engine_slot: &OnceLock<KArc<Engine>>,
) -> Result<KArc<Controller>, InitError> {
    let Queues {
        index,
        admin,
        info,
        io_vector,
        irq,
    } = queues;
    let max_transfer = info
        .max_transfer(cap.min_page_size())
        .map_or(engine::MAX_XFER, |mdts| mdts.min(engine::MAX_XFER));
    let pairs = queue_pairs(&admin)?;
    let io_ring = create_pair(
        &admin,
        regs,
        cap,
        IO_QID,
        queue_depth(cap, IO_DEPTH),
        Some(io_vector),
    )?;
    let panic = panic_queue(
        &admin,
        regs,
        cap,
        pairs,
        info.volatile_write_cache,
        max_transfer,
    )?;
    let engine = start_engine(io_ring, max_transfer)?;
    engine_slot.call_once(|| KArc::clone(&engine));
    let controller = new_controller(ControllerParts {
        index,
        regs: regs.clone(),
        info,
        admin,
        engine,
        panic,
        irq,
    })?;
    Ok(controller)
}

fn queue_depth(cap: Cap, want: u16) -> u16 {
    want.min(cap.max_queue_entries().min(u32::from(u16::MAX)) as u16)
}

/// Lent once the controller that keeps it exists, so nothing after can fail
/// and free memory the drive is using, and last, so a controller that never
/// answers the grant still serves its namespaces.
#[inline(never)]
fn lend_host_memory(controller: &Controller) {
    let Some(buffer) = HostMemoryBuffer::grant(&controller.admin, controller.info.hmb) else {
        return;
    };
    match controller.host_memory.lock() {
        Ok(mut lent) => *lent = Some(buffer),
        // The controller holds it now: never freed if it cannot be recorded.
        Err(_) => core::mem::forget(buffer),
    }
}

#[inline(never)]
fn admin_queue(regs: &MmioRegion, cap: Cap) -> Result<KArc<AdminQueue>, InitError> {
    let depth = queue_depth(cap, ADMIN_DEPTH);
    let ring = Ring::new(regs, cap, ADMIN_QID, depth).ok_or(InitError::NoMemory)?;
    let admin = KArc::try_init(AdminQueue::init(ring)).map_err(|_| InitError::NoMemory)?;
    let depth = admin.depth();
    regs.write::<u32>(regs::AQA, regs::aqa(depth, depth));
    regs.write::<u64>(regs::ASQ, admin.sq_phys());
    regs.write::<u64>(regs::ACQ, admin.cq_phys());
    Ok(admin)
}

#[inline(never)]
fn identify_controller(admin: &AdminQueue) -> Result<ControllerInfo, InitError> {
    let id = admin_step(
        "identify controller",
        admin.identify(Command::identify(cns::CONTROLLER, 0)),
    )?;
    let info = ControllerInfo::parse(&id).ok_or(InitError::Unsupported("identify data"))?;
    if !info.takes_entry_sizes(SQE_BYTES, CQE_BYTES) {
        return Err(InitError::Unsupported("queue entry sizes"));
    }
    Ok(info)
}

/// Ask for the I/O queue and the panic queue; the answer is how many pairs
/// the controller granted.
#[inline(never)]
fn queue_pairs(admin: &AdminQueue) -> Result<u32, InitError> {
    let granted = admin_step(
        "number of queues",
        admin.run(Command::set_queue_count(2, 2)),
    )?;
    Ok(((granted.result & 0xFFFF) + 1).min((granted.result >> 16) + 1))
}

/// The engine gets no more slots than the ring holds beside the requests a
/// timeout may leave in it.
#[inline(never)]
fn start_engine(io_ring: Ring, max_transfer: usize) -> Result<KArc<Engine>, InitError> {
    let slots = (usize::from(io_ring.depth()) - 1)
        .saturating_sub(engine::QUARANTINE_SLOTS)
        .min(IO_SLOTS);
    if slots == 0 {
        return Err(InitError::Unsupported("I/O queue too shallow"));
    }
    let engine = KArc::try_init(Engine::init(
        "nvme",
        slots,
        max_transfer,
        IO_TIMEOUT_MS,
        engine::SLOT_WAIT_MS,
    ))
    .map_err(|_| InitError::NoMemory)?;
    if !engine.prime() {
        return Err(InitError::NoMemory);
    }
    let queue: KBox<dyn engine::QueueOps> =
        KBox::try_new(IoQueue::new(io_ring)).map_err(|_| InitError::NoMemory)?;
    engine.start(queue);
    Ok(engine)
}

/// What an enabled, identified controller brings to its I/O queues.
struct Queues {
    index: usize,
    admin: KArc<AdminQueue>,
    info: ControllerInfo,
    io_vector: u16,
    irq: IrqMechanism,
}

struct ControllerParts {
    index: usize,
    regs: MmioRegion,
    info: ControllerInfo,
    admin: KArc<AdminQueue>,
    engine: KArc<Engine>,
    panic: Option<KBox<PanicQueue>>,
    irq: IrqMechanism,
}

#[inline(never)]
fn new_controller(parts: ControllerParts) -> Result<KArc<Controller>, InitError> {
    let ControllerParts {
        index,
        regs,
        info,
        admin,
        engine,
        panic,
        irq,
    } = parts;
    KArc::try_init(init_struct_with(
        move |slot: SlotPtr<Controller>| -> Result<Initialised<Controller>, AllocError> {
            write_field!(slot, index, index);
            write_field!(slot, regs, regs);
            write_field!(slot, info, info);
            write_field!(slot, admin, admin);
            write_field!(slot, engine, engine);
            write_field!(slot, panic, panic);
            write_init_field!(
                slot,
                host_memory,
                Mutex::init_owned(None, lock_class!("Nvme.host_memory", LOCK_LEVEL_RESOURCE))
            )?;
            write_field!(slot, _irq, irq);
            write_field!(slot, shut_down, AtomicBool::new(false));
            Ok(slot.finish())
        },
    ))
    .map_err(|_| InitError::NoMemory)
}

/// The polled pair kept for the panic path, when the controller granted a
/// second.
#[inline(never)]
fn panic_queue(
    admin: &AdminQueue,
    regs: &MmioRegion,
    cap: Cap,
    pairs: u32,
    flushes: bool,
    max_transfer: usize,
) -> Result<Option<KBox<PanicQueue>>, InitError> {
    if pairs < 2 {
        klog_info!("nvme: one I/O queue pair granted; no panic queue");
        return Ok(None);
    }
    let ring = create_pair(
        admin,
        regs,
        cap,
        PANIC_QID,
        queue_depth(cap, PANIC_DEPTH),
        None,
    )?;
    let queue = PanicQueue::new(ring, flushes, max_transfer).ok_or(InitError::NoMemory)?;
    KBox::try_new(queue)
        .map(Some)
        .map_err(|_| InitError::NoMemory)
}

#[inline(never)]
fn register_namespace(controller: &KArc<Controller>, nsid: u32) {
    let name = DiskName::nvme(controller.index as u32, nsid);
    let id = match controller
        .admin
        .identify(Command::identify(cns::NAMESPACE, nsid))
    {
        Ok(id) => id,
        Err(e) => {
            klog_info!("nvme: {} not identified: {:?}", name, e);
            return;
        }
    };
    let ns = match NamespaceInfo::parse(&id).and_then(|ns| ns.check(PAGE_SIZE).map(|()| ns)) {
        Ok(ns) => ns,
        Err(refusal) => {
            klog_info!("nvme: {} not served: {:?}", name, refusal);
            return;
        }
    };
    let Ok(disk) = KArc::try_new(EngineDisk::new(
        KArc::clone(&controller.engine),
        nsid,
        ns.block_size(),
        ns.capacity_bytes(),
        controller.info.volatile_write_cache,
    )) else {
        return;
    };
    klog_info!(
        "nvme: {} ready, {} MB in {}-byte blocks, volatile cache={}",
        name,
        ns.capacity_bytes() / (1024 * 1024),
        ns.block_size(),
        controller.info.volatile_write_cache,
    );
    block::register_disk(name, disk);
}

impl DeviceShutdown for Controller {
    /// Stop taking I/O, take the host memory back, delete the queues and
    /// notify: once CSTS says the shutdown is complete, power may go.
    fn shutdown(&self) {
        if self.shut_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let csts = self.csts();
        if csts.absent() || csts.fatal() {
            return;
        }
        self.engine.stop();
        crate::hpet::spin_until(&mut || self.engine.is_idle(), DRAIN_MS);

        if let Ok(mut lent) = self.host_memory.lock()
            && let Some(hmb) = lent.take()
            && let Err(kept) = hmb.reclaim(&self.admin)
        {
            klog_info!("nvme{}: host memory buffer not taken back", self.index);
            *lent = Some(kept);
        }
        for qid in [PANIC_QID, IO_QID] {
            let _ = self.admin.run_polled(Command::delete_io_sq(qid));
            let _ = self.admin.run_polled(Command::delete_io_cq(qid));
        }

        let cc = self.regs.read::<u32>(regs::CC);
        self.regs.write::<u32>(regs::CC, regs::cc_shutdown(cc));
        let timeout = (self.info.rtd3e_us / 1000).clamp(SHUTDOWN_FLOOR_MS, SHUTDOWN_CEILING_MS);
        if !crate::hpet::spin_until(&mut || self.csts().shutdown_complete(), timeout) {
            klog_info!(
                "nvme{}: shutdown did not complete in {} ms",
                self.index,
                timeout
            );
        }
    }
}

crate::pci_driver! {
    pub static NVME_DRIVER = {
        name: "nvme",
        match_table: &[PciMatch::ClassSubclass {
            class: CLASS_MASS_STORAGE,
            subclass: SUBCLASS_NVM,
        }],
        probe: nvme_probe,
    };
}
