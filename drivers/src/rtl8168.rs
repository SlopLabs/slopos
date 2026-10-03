//! Realtek RTL8111/8168 gigabit Ethernet. The register sequences, the
//! version table and the ring bookkeeping live in `slopos-rtl8168-core`;
//! this file supplies MMIO, DMA memory, the interrupt and the `NetDevice`.

use core::sync::atomic::{AtomicBool, Ordering, fence};

use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_mm::paging_defs::PAGE_SIZE_4KB_USIZE;
use slopos_net::netdev::{NetDevice, NetDeviceFeatures, NetDeviceStats};
use slopos_net::packetbuf::PacketBuf;
use slopos_net::pool::PacketPool;
use slopos_net::types::{MacAddr, NetError};
use slopos_ostd::sync::{
    InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, SpinLock,
};
use slopos_ostd::{KArc, KVec, klog_info, lock_class};
use slopos_rtl8168_core::chip;
use slopos_rtl8168_core::desc::{DESCRIPTOR_SIZE, OWN, RING_ALIGN};
use slopos_rtl8168_core::regs::{INTR_LINK_CHANGE, INTR_TX_ERROR, TX_CONFIG};
use slopos_rtl8168_core::ring::MIN_FRAME_LEN;
use slopos_rtl8168_core::{
    Descriptor, DescriptorMemory, Probed, Received, RegisterBus, RingAddresses, RxRing, TxError,
    TxRing,
};

use crate::driver_core::msi::{self as core_msi, IrqMechanism};
use crate::driver_core::shutdown::{self, DeviceShutdown};
use crate::pci::{
    BoundDevice, PciMatch, PciProbeError, ProbeOutcome, disable_aspm, disable_bus_master,
    enable_bus_master, enable_memory_space,
};
use crate::pci_defs::PciDeviceInfo;

const PCI_VENDOR_REALTEK: u16 = 0x10ec;
const PCI_DEVICE_RTL8168: u16 = 0x8168;

const REGISTER_WINDOW: usize = 256;
const RX_SLOTS: usize = 256;
const TX_SLOTS: usize = 64;
const BUFFER_LEN: usize = 2048;
const BUFFERS_PER_PAGE: usize = 4096 / BUFFER_LEN;
const MTU: u16 = 1500;

struct Regs<'a>(&'a MmioRegion);

impl RegisterBus for Regs<'_> {
    fn read8(&mut self, offset: usize) -> u8 {
        self.0.read::<u8>(offset)
    }
    fn read16(&mut self, offset: usize) -> u16 {
        self.0.read::<u16>(offset)
    }
    fn read32(&mut self, offset: usize) -> u32 {
        self.0.read::<u32>(offset)
    }
    fn write8(&mut self, offset: usize, value: u8) {
        self.0.write::<u8>(offset, value)
    }
    fn write16(&mut self, offset: usize, value: u16) {
        self.0.write::<u16>(offset, value)
    }
    fn write32(&mut self, offset: usize, value: u32) {
        self.0.write::<u32>(offset, value)
    }
    fn delay_us(&mut self, us: u32) {
        crate::hpet::delay_ns(u64::from(us) * 1000);
    }
}

const _: () = assert!(PAGE_SIZE_4KB_USIZE.is_multiple_of(RING_ALIGN));

/// One page of descriptors.
struct DescriptorPage {
    page: OwnedPageFrame,
    slots: usize,
}

impl DescriptorPage {
    fn alloc(slots: usize) -> Option<Self> {
        Some(Self {
            page: OwnedPageFrame::alloc_zeroed()?,
            slots,
        })
    }

    fn phys(&self) -> u64 {
        self.page.phys_u64()
    }
}

impl DescriptorMemory for DescriptorPage {
    fn slots(&self) -> usize {
        self.slots
    }

    fn read(&self, index: usize) -> Descriptor {
        let at = index * DESCRIPTOR_SIZE;
        let opts1 = self.page.read_volatile_at::<u32>(at).unwrap_or(OWN);
        fence(Ordering::Acquire);
        Descriptor {
            opts1,
            opts2: self.page.read_volatile_at::<u32>(at + 4).unwrap_or(0),
            addr: self.page.read_volatile_at::<u64>(at + 8).unwrap_or(0),
        }
    }

    fn write(&mut self, index: usize, desc: Descriptor) {
        let at = index * DESCRIPTOR_SIZE;
        self.page.write_volatile_at::<u64>(at + 8, desc.addr);
        self.page.write_volatile_at::<u32>(at + 4, desc.opts2);
        fence(Ordering::Release);
        self.page.write_volatile_at::<u32>(at, desc.opts1);
    }
}

/// Per-slot packet buffers, `BUFFERS_PER_PAGE` to a page.
struct Buffers {
    pages: KVec<OwnedPageFrame>,
}

impl Buffers {
    fn alloc(slots: usize) -> Option<Self> {
        let mut pages = KVec::new();
        for _ in 0..slots.div_ceil(BUFFERS_PER_PAGE) {
            pages.push(OwnedPageFrame::alloc_zeroed()?).ok()?;
        }
        Some(Self { pages })
    }

    fn locate(&self, slot: usize) -> Option<(&OwnedPageFrame, usize)> {
        let page = self.pages.get(slot / BUFFERS_PER_PAGE)?;
        Some((page, (slot % BUFFERS_PER_PAGE) * BUFFER_LEN))
    }

    fn addr(&self, slot: usize) -> u64 {
        self.locate(slot)
            .map_or(0, |(page, offset)| page.phys_u64() + offset as u64)
    }
}

struct Nic {
    info: PciDeviceInfo,
    regs: MmioRegion,
    probed: Probed,
    rx: RxRing<DescriptorPage>,
    rx_buffers: Buffers,
    tx: TxRing<DescriptorPage>,
    tx_buffers: Buffers,
    up: bool,
    _irq: IrqMechanism,
}

impl Nic {
    fn ring_addresses(&self) -> RingAddresses {
        RingAddresses {
            tx: self.tx.memory().phys(),
            rx: self.rx.memory().phys(),
            rx_buffer_len: self.rx.buffer_len(),
        }
    }

    fn bring_up(&mut self) -> Result<(), slopos_rtl8168_core::Error> {
        let rings = self.ring_addresses();
        let Self {
            regs,
            probed,
            rx,
            tx,
            info,
            ..
        } = self;
        let stalls = chip::up(&mut Regs(regs), probed, rings, info.function, || {
            rx.rearm();
            tx.clear();
        })?;
        log_stalls("bring-up", stalls);
        self.up = true;
        LINK_UP.store(chip::link(&mut Regs(&self.regs)).up, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) {
        self.up = false;
        match chip::stop(&mut Regs(&self.regs)) {
            Ok(stalls) => log_stalls("stop", stalls),
            Err(e) => klog_info!("rtl8168: stop: {:?}", e),
        }
        self.tx.clear();
    }

    /// Reclaim sent frames, and ring TxPoll again while frames remain: this
    /// chip family ignores a TxPoll that lands while it serves the last one.
    fn reclaim_tx(&mut self) {
        if self.tx.reclaim() > 0 && self.tx.in_flight() > 0 {
            chip::kick_tx(&mut Regs(&self.regs));
        }
    }
}

fn log_stalls(during: &str, stalls: chip::Stalls) {
    for wait in stalls.iter() {
        klog_info!("rtl8168: {}: {:?} timed out; carrying on", during, wait);
    }
}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static NIC: SpinLock<Option<Nic>> =
    SpinLock::new(None, lock_class!("RTL8168_STATE", LOCK_LEVEL_RESOURCE));

/// Serialises bring-up and stop, which take the [`Nic`] out of [`NIC`] and
/// run unlocked: they busy-wait for milliseconds, `chip::up` for hundreds.
static CONTROL: Mutex<()> = Mutex::new((), lock_class!("RTL8168_CONTROL", LOCK_LEVEL_REGISTRY));

/// A killed task still gets the lock, so `set_down` keeps its contract for
/// a dying caller; nothing under it sleeps, so its acquire spins.
fn control() -> MutexGuard<'static, ()> {
    match CONTROL.lock() {
        Ok(guard) => guard,
        Err(_) => loop {
            if let Some(guard) = CONTROL.try_lock() {
                break guard;
            }
            core::hint::spin_loop();
        },
    }
}

/// Take the [`Nic`] out of [`NIC`] if `pick` accepts it, `work` on it with
/// the lock released, and put it back. Meanwhile the data path finds no
/// `Nic` and refuses.
#[inline(never)]
fn with_nic_unlocked(pick: impl FnOnce(&mut Nic) -> bool, work: impl FnOnce(&mut Nic)) {
    let _control = control();
    let mut taken = NIC.lock().take_if(pick);
    if let Some(nic) = taken.as_mut() {
        work(nic);
        *NIC.lock() = taken;
    }
}

/// Link state as of the last carrier sample: `carrier()` must not take a
/// lock.
static LINK_UP: AtomicBool = AtomicBool::new(false);

/// Relaxed atomics: `stats()` answers a query that must not take the driver
/// lock. Byte counts exclude the FCS and transmit padding.
mod counters {
    use core::sync::atomic::{AtomicU64, Ordering};

    pub static RX_PACKETS: AtomicU64 = AtomicU64::new(0);
    pub static TX_PACKETS: AtomicU64 = AtomicU64::new(0);
    pub static RX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static TX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static RX_ERRORS: AtomicU64 = AtomicU64::new(0);
    pub static TX_ERRORS: AtomicU64 = AtomicU64::new(0);
    pub static RX_DROPPED: AtomicU64 = AtomicU64::new(0);
    pub static TX_DROPPED: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

const PADDING: [u8; MIN_FRAME_LEN] = [0; MIN_FRAME_LEN];

pub struct Rtl8168Dev {
    mac: MacAddr,
}

impl NetDevice for Rtl8168Dev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let mut guard = NIC.lock();
        let Some(nic) = guard.as_mut().filter(|n| n.up) else {
            counters::bump(&counters::TX_DROPPED, 1);
            return Err(NetError::NoBufferSpace);
        };
        if !LINK_UP.load(Ordering::Acquire) {
            counters::bump(&counters::TX_DROPPED, 1);
            return Err(NetError::NoBufferSpace);
        }
        let frame = pkt.payload();
        let len = frame.len();
        let wire = len.max(MIN_FRAME_LEN);
        nic.tx.reclaim();
        let Nic {
            tx,
            tx_buffers,
            regs,
            ..
        } = nic;
        let sent = tx.send(wire, |slot| {
            if let Some((page, offset)) = tx_buffers.locate(slot) {
                page.write_slice(offset, frame);
                page.write_slice(offset + len, &PADDING[..wire - len]);
            }
        });
        match sent {
            Ok(_) => {
                fence(Ordering::Release);
                chip::kick_tx(&mut Regs(regs));
                counters::bump(&counters::TX_PACKETS, 1);
                counters::bump(&counters::TX_BYTES, len as u64);
                Ok(())
            }
            Err(TxError::Full) => {
                counters::bump(&counters::TX_DROPPED, 1);
                Err(NetError::NoBufferSpace)
            }
            Err(TxError::Length) => {
                counters::bump(&counters::TX_ERRORS, 1);
                Err(NetError::NoBufferSpace)
            }
        }
    }

    fn poll_tx(&self) {
        if let Some(nic) = NIC.lock().as_mut().filter(|n| n.up) {
            nic.reclaim_tx();
        }
    }

    fn poll_rx(&self, budget: usize, pool: &'static PacketPool) -> KVec<PacketBuf> {
        let mut guard = NIC.lock();
        let Some(nic) = guard.as_mut().filter(|n| n.up) else {
            return KVec::new();
        };
        let status = chip::ack_interrupts(&mut Regs(&nic.regs));
        if status != 0xffff {
            if status & INTR_TX_ERROR != 0 {
                counters::bump(&counters::TX_ERRORS, 1);
            }
            if status & INTR_LINK_CHANGE != 0 {
                LINK_UP.store(chip::link(&mut Regs(&nic.regs)).up, Ordering::Release);
            }
        }
        nic.reclaim_tx();

        let mut packets = KVec::with_capacity(budget.min(64)).unwrap_or_else(|_| KVec::new());
        let Nic { rx, rx_buffers, .. } = nic;
        for _ in 0..budget {
            let received = rx.receive(|slot, len| {
                let (page, offset) = rx_buffers.locate(slot)?;
                let pkt = PacketBuf::from_raw_copy_in(pool, page.slice_at(offset, len)?)?;
                Some((pkt, len))
            });
            match received {
                None => break,
                Some(Received::Frame(Some((pkt, len)))) => {
                    if packets.push(pkt).is_ok() {
                        counters::bump(&counters::RX_PACKETS, 1);
                        counters::bump(&counters::RX_BYTES, len as u64);
                    } else {
                        counters::bump(&counters::RX_DROPPED, 1);
                    }
                }
                Some(Received::Frame(None)) => counters::bump(&counters::RX_DROPPED, 1),
                Some(Received::Dropped(_)) => counters::bump(&counters::RX_ERRORS, 1),
            }
        }
        packets
    }

    fn set_up(&self) {
        with_nic_unlocked(
            |n| !n.up,
            |nic| {
                if let Err(e) = nic.bring_up() {
                    klog_info!("rtl8168: bring-up failed: {:?}", e);
                    nic.stop();
                }
            },
        );
    }

    fn set_down(&self) {
        with_nic_unlocked(
            |n| n.up,
            |nic| {
                nic.stop();
                LINK_UP.store(false, Ordering::Release);
            },
        );
    }

    fn mtu(&self) -> u16 {
        MTU
    }

    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn stats(&self) -> NetDeviceStats {
        let mut out = NetDeviceStats::new();
        out.rx_packets = counters::RX_PACKETS.load(Ordering::Relaxed);
        out.tx_packets = counters::TX_PACKETS.load(Ordering::Relaxed);
        out.rx_bytes = counters::RX_BYTES.load(Ordering::Relaxed);
        out.tx_bytes = counters::TX_BYTES.load(Ordering::Relaxed);
        out.rx_errors = counters::RX_ERRORS.load(Ordering::Relaxed);
        out.tx_errors = counters::TX_ERRORS.load(Ordering::Relaxed);
        out.rx_dropped = counters::RX_DROPPED.load(Ordering::Relaxed);
        out.tx_dropped = counters::TX_DROPPED.load(Ordering::Relaxed);
        out
    }

    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }

    fn carrier(&self) -> bool {
        LINK_UP.load(Ordering::Acquire)
    }

    fn carrier_detect(&self) -> bool {
        true
    }

    fn rx_pending(&self) -> bool {
        NIC.lock().as_ref().is_some_and(|n| n.up && n.rx.pending())
    }

    fn sample_carrier(&self) {
        let nic = NIC.lock();
        let up = nic
            .as_ref()
            .filter(|n| n.up)
            .is_some_and(|n| chip::link(&mut Regs(&n.regs)).up);
        LINK_UP.store(up, Ordering::Release);
    }
}

/// On poweroff or reboot the chip stops DMA into memory the next owner
/// will reuse, and stops interrupting.
struct ShutdownHook;

impl DeviceShutdown for ShutdownHook {
    fn shutdown(&self) {
        with_nic_unlocked(
            |_| true,
            |nic| {
                nic.stop();
                disable_bus_master(&nic.info);
            },
        );
    }
}

fn irq_handler(_vector: u8) {
    slopos_net::napi::wake_napi();
}

fn first_memory_bar(info: &PciDeviceInfo) -> Option<u8> {
    (0..info.bar_count).find(|&i| {
        let bar = info.bars[i as usize];
        bar.base != 0 && bar.is_io == 0 && bar.size as usize >= REGISTER_WINDOW
    })
}

#[inline(never)]
fn rings() -> Option<(
    RxRing<DescriptorPage>,
    Buffers,
    TxRing<DescriptorPage>,
    Buffers,
)> {
    let rx_buffers = Buffers::alloc(RX_SLOTS)?;
    let tx_buffers = Buffers::alloc(TX_SLOTS)?;
    let rx = RxRing::new(DescriptorPage::alloc(RX_SLOTS)?, BUFFER_LEN, |i| {
        rx_buffers.addr(i)
    })?;
    let tx = TxRing::new(DescriptorPage::alloc(TX_SLOTS)?, BUFFER_LEN, |i| {
        tx_buffers.addr(i)
    })?;
    Some((rx, rx_buffers, tx, tx_buffers))
}

fn probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    if !DEVICE_CLAIMED.claim() {
        klog_info!("rtl8168: already own a NIC; declining additional device");
        return Ok(ProbeOutcome::Declined);
    }
    let result = attach(bound);
    if !matches!(result, Ok(ProbeOutcome::Bound)) {
        DEVICE_CLAIMED.reset();
    }
    result
}

/// Give the probed chip its rings and interrupt, then [`commission`] it.
#[inline(never)]
fn install(
    bound: &mut BoundDevice<'_>,
    regs: MmioRegion,
    probed: Probed,
    name: &str,
) -> Result<&'static str, PciProbeError> {
    let info = *bound.info();
    let Some((rx, rx_buffers, tx, tx_buffers)) = rings() else {
        return Err(PciProbeError::OutOfMemory);
    };
    let mut vectors = [0u8; 1];
    let Some(irq) = core_msi::setup_interrupts(bound, 1, &mut vectors, irq_handler) else {
        klog_info!("rtl8168: neither MSI-X nor MSI");
        return Err(PciProbeError::Unsupported);
    };
    let irq_kind = match &irq {
        IrqMechanism::Msix { cap, .. } => {
            crate::msix::msix_enable(info.bus, info.device, info.function, cap);
            "MSI-X"
        }
        IrqMechanism::Msi { .. } => "MSI",
    };
    enable_bus_master(&info);

    commission(
        Nic {
            info,
            regs,
            probed,
            rx,
            rx_buffers,
            tx,
            tx_buffers,
            up: false,
            _irq: irq,
        },
        name,
    )?;
    Ok(irq_kind)
}

/// Bring the chip into service and put it in [`NIC`]; on failure the chip
/// is stopped and masters the bus no more before its memory is freed.
#[inline(never)]
fn commission(mut nic: Nic, name: &str) -> Result<(), PciProbeError> {
    if let Err(e) = nic.bring_up() {
        klog_info!("rtl8168: {} bring-up failed: {:?}", name, e);
        nic.stop();
        disable_bus_master(&nic.info);
        return Err(PciProbeError::DeviceFault);
    }
    *NIC.lock() = Some(nic);
    Ok(())
}

#[inline(never)]
fn attach(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    let Some(bar) = first_memory_bar(&info) else {
        klog_info!("rtl8168: no memory BAR");
        return Err(PciProbeError::Unsupported);
    };
    enable_memory_space(&info);
    let regs = bound
        .map_bar(bar, 0, REGISTER_WINDOW)
        .map_err(|_| PciProbeError::OutOfMemory)?
        .clone();

    let tx_config = regs.read::<u32>(TX_CONFIG);
    let Some(version) = slopos_rtl8168_core::identify(tx_config) else {
        klog_info!(
            "rtl8168: {:02x}:{:02x}.{} XID {:03x} is not a version this driver knows; declined",
            info.bus,
            info.device,
            info.function,
            slopos_rtl8168_core::xid(tx_config)
        );
        return Ok(ProbeOutcome::Declined);
    };

    let aspm = if chip::aspm_validated(&mut Regs(&regs)) {
        "left as firmware set it: the vendor validated it"
    } else if disable_aspm(&info) {
        "L0s and L1 off"
    } else {
        "untouched: no PCI Express capability"
    };
    klog_info!("rtl8168: ASPM {}", aspm);

    // Firmware may have left the chip receiving into memory since reused:
    // it masters the bus again only after the reset.
    disable_bus_master(&info);
    let (probed, stalls) = chip::probe(&mut Regs(&regs)).map_err(|e| {
        klog_info!("rtl8168: {} left unused: {:?}", version.name, e);
        PciProbeError::DeviceFault
    })?;
    log_stalls("probe", stalls);

    let irq_kind = install(bound, regs, probed, version.name)?;

    match KArc::try_new(Rtl8168Dev {
        mac: MacAddr(probed.mac),
    }) {
        Ok(dev) => {
            let dev: KArc<dyn NetDevice + Send + Sync> = dev;
            if slopos_net::nic::publish(dev).is_some() {
                slopos_net::napi::wake_napi();
            } else {
                klog_info!("rtl8168: not published to the network stack");
            }
        }
        Err(_) => klog_info!("rtl8168: alloc failed; not published"),
    }
    match KArc::try_new(ShutdownHook) {
        Ok(hook) => {
            let hook: KArc<dyn DeviceShutdown> = hook;
            if !shutdown::register(hook) {
                klog_info!("rtl8168: will not be told of a poweroff");
            }
        }
        Err(_) => klog_info!("rtl8168: will not be told of a poweroff"),
    }

    let mac = probed.mac;
    klog_info!(
        "rtl8168: {} at {:02x}:{:02x}.{} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} irq {}",
        version.name,
        info.bus,
        info.device,
        info.function,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        irq_kind,
    );
    Ok(ProbeOutcome::Bound)
}

crate::pci_driver! {
    pub static RTL8168_DRIVER = {
        name: "rtl8168",
        match_table: &[PciMatch::VendorDevice {
            vendor: PCI_VENDOR_REALTEK,
            device: PCI_DEVICE_RTL8168,
        }],
        probe: probe,
    };
}
