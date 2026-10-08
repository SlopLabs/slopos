//! virtio-blk: one request virtqueue per device, served through the block
//! layer's request engine. A request is a `1 + N + 1` descriptor chain — the
//! header, the payload pages, the status byte the device writes.

use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_fs::blockdev::BlockDevice;
use slopos_mm::mmio::MmioRegion;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::{KArc, KBox, klog_debug, klog_info, write_field};

use crate::block::engine::{self, BlkError, Engine, Op, QueueOps, Request, RequestPages};
use crate::block::{self, DiskName, EngineDisk};
use crate::pci::BoundDevice;
use crate::pci::{PciMatch, PciProbeError, ProbeOutcome};
use crate::virtio::{
    self, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VirtioMmioCaps,
    VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok, setup_interrupts,
    },
    queue::{self, MAX_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
/// Valid only once `VIRTIO_BLK_F_FLUSH` has been negotiated.
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;
/// Written before submission: reading back as this means the device never
/// wrote a status byte at all.
const STATUS_PENDING: u8 = 0xFF;

/// The device reports its logical block size in `blk_size`.
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
/// The device has a write-back cache and honours `VIRTIO_BLK_T_FLUSH`.
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

/// Request headers address the medium in these, whatever `blk_size` says.
const SECTOR_SIZE: u64 = 512;
const CONFIG_CAPACITY: usize = 0;
const CONFIG_BLK_SIZE: usize = 20;

/// The device answers in microseconds; one that has not in this long has
/// lost the request.
const REQUEST_TIMEOUT_MS: u64 = 5000;

/// Four slots of a `1 + 32 + 1` descriptor chain each: 136 of
/// [`BLK_QUEUE_SIZE`], leaving room for the chains a timeout may quarantine
/// and the device has not yet returned.
const REQUEST_SLOTS: usize = 4;
const MAX_CHAIN_DESCS: usize = 2 + engine::MAX_DATA_PAGES;
const BLK_QUEUE_SIZE: u16 = MAX_QUEUE_SIZE;
const STATUS_OFFSET: usize = size_of::<VirtioBlkReqHeader>();
/// The end of a submitted chain.
const NO_DESC: u16 = u16::MAX;
/// A descriptor in no submitted chain.
const UNUSED_DESC: u16 = u16::MAX - 1;

const _: () =
    assert!(REQUEST_SLOTS * MAX_CHAIN_DESCS + 2 * MAX_CHAIN_DESCS <= BLK_QUEUE_SIZE as usize);

#[repr(C)]
#[derive(Clone, Copy, slopos_ostd::Pod)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[derive(slopos_ostd::SlotFields)]
struct VirtioBlkQueue {
    vq: Virtqueue,
    notify_cfg: MmioRegion,
    notify_off_multiplier: u32,
    /// Each submitted descriptor's successor in its chain, kept in kernel
    /// memory: a retired chain is freed without trusting the device-visible
    /// `next` fields, and a head retired twice frees nothing the second time.
    chain_next: [u16; MAX_QUEUE_SIZE as usize],
    /// The MSI-X table this queue's vector is programmed into, kept for the
    /// device's life.
    msix_state: Option<VirtioMsixState>,
}

impl VirtioBlkQueue {
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, vq, Virtqueue::new());
                write_field!(slot, notify_cfg, MmioRegion::empty());
                write_field!(slot, notify_off_multiplier, 0);
                write_field!(slot, chain_next, [UNUSED_DESC; MAX_QUEUE_SIZE as usize]);
                write_field!(slot, msix_state, None);
                Ok(slot.finish())
            },
        )
    }

    /// All or nothing: a partial reservation is handed back, so a failure
    /// leaks no descriptors.
    fn alloc_chain(&mut self, count: usize) -> Option<[u16; MAX_CHAIN_DESCS]> {
        let mut descs = [0u16; MAX_CHAIN_DESCS];
        for i in 0..count {
            match self.vq.alloc_desc() {
                Some(desc) => descs[i] = desc,
                None => {
                    for &desc in &descs[..i] {
                        self.vq.free_desc(desc);
                    }
                    return None;
                }
            }
        }
        Some(descs)
    }
}

impl QueueOps for VirtioBlkQueue {
    fn submit(&mut self, req: &Request, pages: &RequestPages) -> Result<u16, BlkError> {
        if !self.vq.is_ready() {
            return Err(BlkError::NotReady);
        }
        let (type_, sector) = match req.op {
            Op::Read => (VIRTIO_BLK_T_IN, req.offset / SECTOR_SIZE),
            Op::Write => (VIRTIO_BLK_T_OUT, req.offset / SECTOR_SIZE),
            // The flush sector field must be zero.
            Op::Flush => (VIRTIO_BLK_T_FLUSH, 0),
        };
        let header = VirtioBlkReqHeader {
            type_,
            reserved: 0,
            sector,
        };
        if !pages.aux.write_at(0, &header)
            || !pages
                .aux
                .write_volatile_at::<u8>(STATUS_OFFSET, STATUS_PENDING)
        {
            return Err(BlkError::BadRequest);
        }

        let data = pages.span(req.len);
        let count = 2 + data.len();
        let descs = self.alloc_chain(count).ok_or(BlkError::Busy)?;
        let req_phys = pages.aux.phys_u64();
        self.vq.write_desc(
            descs[0],
            VirtqDesc {
                addr: req_phys,
                len: size_of::<VirtioBlkReqHeader>() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: descs[1],
            },
        );
        for (i, page) in data.iter().enumerate() {
            let flags = if req.op == Op::Read {
                VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
            } else {
                VIRTQ_DESC_F_NEXT
            };
            self.vq.write_desc(
                descs[1 + i],
                VirtqDesc {
                    addr: page.phys_u64(),
                    len: (req.len - i * engine::PAGE_SIZE).min(engine::PAGE_SIZE) as u32,
                    flags,
                    next: descs[2 + i],
                },
            );
        }
        self.vq.write_desc(
            descs[count - 1],
            VirtqDesc {
                addr: req_phys + STATUS_OFFSET as u64,
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        for i in 0..count {
            self.chain_next[usize::from(descs[i])] =
                if i + 1 < count { descs[i + 1] } else { NO_DESC };
        }

        self.vq.submit(descs[0]);
        queue::notify_queue(&self.notify_cfg, self.notify_off_multiplier, &self.vq, 0);
        Ok(descs[0])
    }

    fn pop(&mut self) -> Option<(u16, u32)> {
        self.vq.try_pop_used().map(|elem| (elem.id as u16, 0))
    }

    fn retire(&mut self, head: u16) {
        let mut desc = head;
        while desc < self.vq.size && self.chain_next[usize::from(desc)] != UNUSED_DESC {
            let next = core::mem::replace(&mut self.chain_next[usize::from(desc)], UNUSED_DESC);
            self.vq.free_desc(desc);
            desc = next;
        }
    }

    fn outcome(&self, pages: &RequestPages, _status: u32) -> Result<(), BlkError> {
        match pages
            .aux
            .read_volatile_at::<u8>(STATUS_OFFSET)
            .unwrap_or(STATUS_PENDING)
        {
            VIRTIO_BLK_S_OK => Ok(()),
            VIRTIO_BLK_S_UNSUPP => Err(BlkError::Unsupported),
            STATUS_PENDING => Err(BlkError::DeviceFault { retry: true }),
            _ => Err(BlkError::DeviceFault { retry: false }),
        }
    }
}

/// virtio disks are lettered in probe order: `vda`, `vdb`, …
static NEXT_DISK: AtomicUsize = AtomicUsize::new(0);

fn read_config(caps: &VirtioMmioCaps, features: u64) -> (u64, u32) {
    if !caps.has_device_cfg() {
        return (0, SECTOR_SIZE as u32);
    }
    let lo = u64::from(caps.device_cfg.read::<u32>(CONFIG_CAPACITY));
    let hi = u64::from(caps.device_cfg.read::<u32>(CONFIG_CAPACITY + 4));
    let block = if features & VIRTIO_BLK_F_BLK_SIZE != 0 {
        caps.device_cfg.read::<u32>(CONFIG_BLK_SIZE)
    } else {
        SECTOR_SIZE as u32
    };
    let block = if block.is_power_of_two() && (512..=4096).contains(&block) {
        block
    } else {
        SECTOR_SIZE as u32
    };
    ((lo | (hi << 32)) * SECTOR_SIZE, block)
}

/// Bring the request queue up, hand it to `engine`, and answer the disk's
/// capacity and logical block size.
#[inline(never)]
fn start_queue(
    engine: &Engine,
    caps: VirtioMmioCaps,
    features: u64,
    msix_state: Option<VirtioMsixState>,
) -> Result<(u64, u32), PciProbeError> {
    let q0_msix_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));
    let mut queue =
        KBox::try_init(VirtioBlkQueue::init_empty()).map_err(|_| PciProbeError::OutOfMemory)?;
    if !queue::setup_queue_into(
        &caps.common_cfg,
        0,
        BLK_QUEUE_SIZE,
        q0_msix_entry,
        &mut queue.vq,
    ) {
        klog_info!("virtio-blk: queue setup failed");
        return Err(PciProbeError::OutOfMemory);
    }
    set_driver_ok(&caps);

    // The const assert covers `BLK_QUEUE_SIZE`, but the device may negotiate
    // down; say so once here rather than per exhausted request.
    let ring_size = queue.vq.free_count();
    if usize::from(ring_size) < REQUEST_SLOTS * MAX_CHAIN_DESCS {
        klog_info!(
            "virtio-blk: virtqueue negotiated down to {} descriptors, below the {} the {} request slots want — expect Busy under load",
            ring_size,
            REQUEST_SLOTS * MAX_CHAIN_DESCS,
            REQUEST_SLOTS
        );
    }

    let (capacity, block_size) = read_config(&caps, features);
    queue.notify_cfg = caps.notify_cfg;
    queue.notify_off_multiplier = caps.notify_off_multiplier;
    queue.msix_state = msix_state;
    engine.start(queue);
    Ok((capacity, block_size))
}

fn virtio_blk_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    klog_info!(
        "virtio-blk: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(&info);
    let caps = parse_capabilities(&info);
    klog_debug!(
        "virtio-blk: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );
    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        return Err(PciProbeError::Unsupported);
    }

    let negotiated = negotiate_features(
        &caps,
        virtio::VIRTIO_F_VERSION_1,
        VIRTIO_BLK_F_FLUSH | VIRTIO_BLK_F_BLK_SIZE,
    );
    if !negotiated.success {
        klog_info!("virtio-blk: features negotiation failed");
        return Err(PciProbeError::DeviceFault);
    }
    let features = negotiated.driver_features;

    let engine = KArc::try_init(Engine::init(
        "virtio-blk",
        REQUEST_SLOTS,
        engine::MAX_XFER,
        REQUEST_TIMEOUT_MS,
        engine::SLOT_WAIT_MS,
    ))
    .map_err(|_| PciProbeError::OutOfMemory)?;

    // VirtIO modern on q35 always has MSI-X; MSI is the minimum fallback.
    let irq_engine = engine.clone();
    let (irq_mode, msix_state) = setup_interrupts(bound, &caps, 1, move |_queue: u8| {
        irq_engine.handle_irq();
    })
    .unwrap_or_else(|msg| {
        panic!(
            "virtio-blk: {}:{}.{} {}",
            info.bus, info.device, info.function, msg
        )
    });
    #[cfg(feature = "test-hooks")]
    let msix_for_tests = msix_state.clone();

    if !engine.prime() {
        klog_info!("virtio-blk: could not preallocate request slot DMA pages");
        return Err(PciProbeError::OutOfMemory);
    }

    let (capacity, block_size) = start_queue(&engine, caps, features, msix_state)?;
    let flushes = features & VIRTIO_BLK_F_FLUSH != 0;
    let disk = EngineDisk::new(engine, 0, block_size, capacity, flushes);
    let name = register(disk)?;
    klog_info!("virtio-blk: {} irq {:?}", name, irq_mode);
    #[cfg(feature = "test-hooks")]
    test_hooks::record_msix(name, msix_for_tests);
    Ok(ProbeOutcome::Bound)
}

#[inline(never)]
fn register(disk: EngineDisk) -> Result<DiskName, PciProbeError> {
    let name = DiskName::virtio(NEXT_DISK.fetch_add(1, Ordering::Relaxed));
    klog_info!(
        "virtio-blk: {} ready, {} MB in {}-byte blocks",
        name,
        disk.capacity() / (1024 * 1024),
        disk.logical_block_size(),
    );
    let disk = KArc::try_new(disk).map_err(|_| PciProbeError::OutOfMemory)?;
    if !block::register_disk(name, disk) {
        return Err(PciProbeError::OutOfMemory);
    }
    Ok(name)
}

#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use super::DiskName;
    use crate::virtio::VirtioMsixState;
    use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, Mutex};
    use slopos_ostd::{KVec, lock_class};

    static MSIX: Mutex<KVec<(DiskName, Option<VirtioMsixState>)>> = Mutex::new(
        KVec::new(),
        lock_class!("VIRTIO_BLK_TEST_MSIX", LOCK_LEVEL_REGISTRY),
    );

    pub(super) fn record_msix(name: DiskName, state: Option<VirtioMsixState>) {
        if let Ok(mut table) = MSIX.lock() {
            let _ = table.push((name, state));
        }
    }

    /// The MSI-X programming of the virtio disk `name`.
    pub fn msix_state(name: &[u8]) -> Option<VirtioMsixState> {
        match MSIX.lock() {
            Ok(table) => table
                .iter()
                .find(|(n, _)| n.as_bytes() == name)
                .and_then(|(_, s)| s.clone()),
            Err(_) => None,
        }
    }
}

crate::pci_driver! {
    pub static VIRTIO_BLK_DRIVER = {
        name: "virtio-blk",
        match_table: &[
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_LEGACY,
            },
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_MODERN,
            },
        ],
        probe: virtio_blk_probe,
    };
}
