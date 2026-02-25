use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::PhysAddr;
use slopos_lib::ports::{PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA};
use slopos_lib::string::cstr_to_str;
use slopos_lib::{InitFlag, IrqMutex, klog_info};
use slopos_mm::mmio::MmioRegion;

pub use crate::pci_defs::*;

const PCI_SECONDARY_BUS_OFFSET: u8 = 0x19;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciGpuInfo {
    pub present: c_int,
    pub device: PciDeviceInfo,
    pub mmio_phys_base: u64,
    pub mmio_region: MmioRegion,
    pub mmio_size: u64,
}

impl PciGpuInfo {
    pub const fn zeroed() -> Self {
        Self {
            present: 0,
            device: PciDeviceInfo::zeroed(),
            mmio_phys_base: 0,
            mmio_region: MmioRegion::empty(),
            mmio_size: 0,
        }
    }
}

#[repr(C)]
pub struct PciDriver {
    pub name: *const u8,
    pub match_fn: Option<fn(*const PciDeviceInfo, *mut core::ffi::c_void) -> bool>,
    pub probe: Option<fn(*const PciDeviceInfo, *mut core::ffi::c_void) -> c_int>,
    pub context: *mut core::ffi::c_void,
}

unsafe impl Sync for PciDriver {}

struct PciEnumState {
    bus_visited: [u8; PCI_MAX_BUSES],
    devices: [PciDeviceInfo; PCI_MAX_DEVICES],
    device_count: usize,
    primary_gpu: PciGpuInfo,
}

impl PciEnumState {
    const fn new() -> Self {
        Self {
            bus_visited: [0; PCI_MAX_BUSES],
            devices: [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES],
            device_count: 0,
            primary_gpu: PciGpuInfo::zeroed(),
        }
    }
}

struct PciDriverRegistry {
    drivers: [*const PciDriver; PCI_DRIVER_MAX],
    count: usize,
}

impl PciDriverRegistry {
    const fn new() -> Self {
        Self {
            drivers: [ptr::null(); PCI_DRIVER_MAX],
            count: 0,
        }
    }
}

// SAFETY: PciDriverRegistry only stores pointers to 'static PciDrivers
unsafe impl Send for PciDriverRegistry {}

static PCI_INIT: InitFlag = InitFlag::new();
static ENUM_STATE: IrqMutex<PciEnumState> = IrqMutex::new(PciEnumState::new());
static DRIVER_REGISTRY: IrqMutex<PciDriverRegistry> = IrqMutex::new(PciDriverRegistry::new());
static DEVICE_COUNT_CACHE: AtomicUsize = AtomicUsize::new(0);

fn cstr_or_placeholder(ptr: *const u8) -> &'static str {
    unsafe { cstr_to_str(ptr as *const c_char) }
}

#[inline(always)]
fn pci_config_addr(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        PCI_CONFIG_ADDRESS.write(pci_config_addr(bus, device, function, offset));
        PCI_CONFIG_DATA.read()
    }
}

pub fn pci_config_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let value = pci_config_read32(bus, device, function, offset);
    ((value >> ((offset & 0x2) * 8)) & 0xFFFF) as u16
}

pub fn pci_config_read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let value = pci_config_read32(bus, device, function, offset);
    ((value >> ((offset & 0x3) * 8)) & 0xFF) as u8
}

pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        PCI_CONFIG_ADDRESS.write(pci_config_addr(bus, device, function, offset));
        PCI_CONFIG_DATA.write(value);
    }
}

pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let dword = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x2) * 8;
    let mask = !(0xFFFF << shift);
    let new_dword = (dword & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_dword);
}

pub fn pci_config_write8(bus: u8, device: u8, function: u8, offset: u8, value: u8) {
    let dword = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x3) * 8;
    let mask = !(0xFF << shift);
    let new_dword = (dword & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_dword);
}

// =============================================================================
// PCI Capability List Walking
// =============================================================================

/// Iterator over PCI capabilities in a device's configuration space.
///
/// Walks the capability linked list starting from the Capabilities Pointer
/// (offset 0x34). Each capability header contains an 8-bit ID and a pointer
/// to the next capability.
///
/// # Infinite-loop protection
///
/// A guard counter limits traversal to [`Self::MAX_CAPS`] entries to protect
/// against malformed capability lists on buggy hardware.
pub struct PciCapabilityIter {
    bus: u8,
    device: u8,
    function: u8,
    next_ptr: u8,
    /// Remaining entries before we give up (infinite-loop guard).
    remaining: u8,
}

impl PciCapabilityIter {
    /// Maximum capabilities to visit before assuming a malformed list.
    ///
    /// The standard 256-byte config space can fit at most ~60 entries
    /// (4 bytes minimum per capability, starting around offset 0x40).
    /// 48 is a generous upper bound matching Linux's `PCI_FIND_CAP_TTL`.
    const MAX_CAPS: u8 = 48;

    /// Create a capability iterator for the specified PCI function.
    ///
    /// Returns an empty iterator if the device's Status register does not
    /// advertise a capabilities list (bit 4 of Status).
    pub fn new(bus: u8, device: u8, function: u8) -> Self {
        let status = pci_config_read16(bus, device, function, PCI_STATUS_OFFSET);
        let first_ptr = if (status & PCI_STATUS_CAP_LIST) != 0 {
            // PCI spec: bottom 2 bits of the Capabilities Pointer are reserved.
            pci_config_read8(bus, device, function, PCI_CAP_PTR_OFFSET) & 0xFC
        } else {
            0
        };

        Self {
            bus,
            device,
            function,
            next_ptr: first_ptr,
            remaining: Self::MAX_CAPS,
        }
    }

    /// Create a capability iterator for a known [`PciDeviceInfo`].
    pub fn for_device(info: &PciDeviceInfo) -> Self {
        Self::new(info.bus, info.device, info.function)
    }
}

impl Iterator for PciCapabilityIter {
    type Item = PciCapability;

    fn next(&mut self) -> Option<PciCapability> {
        if self.next_ptr == 0 || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let offset = self.next_ptr;
        let id = pci_config_read8(self.bus, self.device, self.function, offset);
        // PCI spec: bottom 2 bits of the Next Pointer are reserved.
        let next = pci_config_read8(self.bus, self.device, self.function, offset + 1) & 0xFC;

        self.next_ptr = next;
        Some(PciCapability { offset, id })
    }
}

/// Find the first PCI capability with the given ID.
///
/// Returns the config-space byte offset of the capability header,
/// or `None` if the device doesn't advertise that capability.
pub fn pci_find_capability(bus: u8, device: u8, function: u8, cap_id: u8) -> Option<u8> {
    PciCapabilityIter::new(bus, device, function)
        .find(|cap| cap.id == cap_id)
        .map(|cap| cap.offset)
}

/// Convenience methods for PCI capability queries on a known device.
impl PciDeviceInfo {
    /// Find the first capability with the given ID for this device.
    pub fn find_capability(&self, cap_id: u8) -> Option<u8> {
        pci_find_capability(self.bus, self.device, self.function, cap_id)
    }

    /// Iterate over all PCI capabilities of this device.
    pub fn capabilities(&self) -> PciCapabilityIter {
        PciCapabilityIter::for_device(self)
    }
}

/// Human-readable name for a PCI capability ID (for boot log output).
fn pci_cap_id_name(id: u8) -> &'static str {
    match id {
        0x01 => "PM",
        0x02 => "AGP",
        0x03 => "VPD",
        0x04 => "SlotID",
        PCI_CAP_ID_MSI => "MSI",
        0x06 => "CompactPCI",
        0x07 => "PCI-X",
        0x08 => "HyperTransport",
        PCI_CAP_ID_VNDR => "Vendor",
        0x0A => "DebugPort",
        0x0B => "CompactPCI-CRC",
        0x0D => "Bridge-SubVID",
        PCI_CAP_ID_PCIE => "PCIe",
        PCI_CAP_ID_MSIX => "MSI-X",
        0x12 => "SATA",
        0x13 => "AF",
        _ => "Unknown",
    }
}

fn pci_read_vendor_id(bus: u8, device: u8, function: u8) -> u16 {
    pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET)
}

fn pci_read_header_type(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_HEADER_TYPE_OFFSET)
}

fn pci_is_multifunction(bus: u8, device: u8) -> bool {
    (pci_read_header_type(bus, device, 0) & 0x80) != 0
}

fn pci_get_secondary_bus(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_SECONDARY_BUS_OFFSET)
}

fn pci_probe_bar(bus: u8, device: u8, function: u8, bar_idx: u8) -> PciBarInfo {
    let bar_offset = PCI_BAR0_OFFSET + bar_idx * 4;
    let original = pci_config_read32(bus, device, function, bar_offset);
    let is_io = (original & 1) != 0;

    pci_config_write32(bus, device, function, bar_offset, 0xFFFF_FFFF);
    let size_mask = pci_config_read32(bus, device, function, bar_offset);
    pci_config_write32(bus, device, function, bar_offset, original);

    if size_mask == 0 || size_mask == 0xFFFF_FFFF {
        return PciBarInfo::zeroed();
    }

    if is_io {
        let base = (original & !0x3) as u64;
        let size = (!((size_mask as u64) | 0xFFFF_FFFF_FFFF_0003) + 1) as u64;
        PciBarInfo {
            base,
            size,
            is_io: 1,
            is_64bit: 0,
            prefetchable: 0,
        }
    } else {
        let is_64bit = ((original >> 1) & 0x3) == 2;
        let is_prefetchable = ((original >> 3) & 1) != 0;
        let base_low = (original & !0xF) as u64;
        let base_high = if is_64bit && bar_idx < 5 {
            pci_config_read32(bus, device, function, bar_offset + 4) as u64
        } else {
            0
        };
        let base = base_low | (base_high << 32);
        let size = (!((size_mask as u64) | 0xF) + 1) as u64;
        PciBarInfo {
            base,
            size,
            is_io: 0,
            is_64bit: is_64bit as u8,
            prefetchable: is_prefetchable as u8,
        }
    }
}

fn pci_probe_device(state: &mut PciEnumState, bus: u8, device: u8, function: u8) {
    let vendor = pci_read_vendor_id(bus, device, function);
    if vendor == 0xFFFF {
        return;
    }

    let device_id = pci_config_read16(bus, device, function, PCI_DEVICE_ID_OFFSET);
    let class = pci_config_read8(bus, device, function, PCI_CLASS_CODE_OFFSET);
    let subclass = pci_config_read8(bus, device, function, PCI_SUBCLASS_OFFSET);
    let prog_if = pci_config_read8(bus, device, function, PCI_PROG_IF_OFFSET);
    let revision = pci_config_read8(bus, device, function, PCI_REVISION_ID_OFFSET);
    let header_type = pci_read_header_type(bus, device, function) & 0x7F;
    let interrupt_line = pci_config_read8(bus, device, function, PCI_INTERRUPT_LINE_OFFSET);
    let interrupt_pin = pci_config_read8(bus, device, function, PCI_INTERRUPT_PIN_OFFSET);

    let mut bars = [PciBarInfo::zeroed(); PCI_MAX_BARS];
    let mut bar_count = 0u8;
    if header_type == 0 {
        let mut bar_idx = 0u8;
        while bar_idx < 6 {
            let bar = pci_probe_bar(bus, device, function, bar_idx);
            bars[bar_idx as usize] = bar;
            if bar.base != 0 || bar.size != 0 {
                bar_count = bar_idx + 1;
            }
            if bar.is_64bit != 0 {
                bar_idx += 1;
            }
            bar_idx += 1;
        }
    }

    // ----- Capability list discovery (single walk) -----
    let mut msi_cap_offset: Option<u8> = None;
    let mut msix_cap_offset: Option<u8> = None;

    for cap in PciCapabilityIter::new(bus, device, function) {
        match cap.id {
            PCI_CAP_ID_MSI if msi_cap_offset.is_none() => msi_cap_offset = Some(cap.offset),
            PCI_CAP_ID_MSIX if msix_cap_offset.is_none() => msix_cap_offset = Some(cap.offset),
            _ => {}
        }
    }

    let info = PciDeviceInfo {
        bus,
        device,
        function,
        vendor_id: vendor,
        device_id,
        class_code: class,
        subclass,
        prog_if,
        revision,
        header_type,
        irq_line: interrupt_line,
        irq_pin: interrupt_pin,
        bar_count,
        bars,
        msi_cap_offset,
        msix_cap_offset,
    };

    if state.device_count < PCI_MAX_DEVICES {
        state.devices[state.device_count] = info;
        state.device_count += 1;
    }

    klog_info!(
        "PCI: [Bus {} Dev {} Func {}] VID=0x{:04x} DID=0x{:04x} Class=0x{:02x}:{:02x} ProgIF=0x{:02x} Rev=0x{:02x}",
        bus,
        device,
        function,
        vendor,
        device_id,
        class,
        subclass,
        prog_if,
        revision
    );

    // Log capabilities (if any)
    for cap in info.capabilities() {
        klog_info!(
            "    CAP: 0x{:02x} ({}) at offset 0x{:02x}",
            cap.id,
            pci_cap_id_name(cap.id),
            cap.offset
        );
    }

    for (i, bar) in bars.iter().enumerate() {
        if bar.base != 0 || bar.size != 0 {
            if bar.is_io != 0 {
                klog_info!("    BAR{}: IO base=0x{:x} size={}", i, bar.base, bar.size);
            } else {
                let pf = if bar.prefetchable != 0 {
                    "prefetch"
                } else {
                    "non-prefetch"
                };
                let bits = if bar.is_64bit != 0 { "64bit" } else { "32bit" };
                klog_info!(
                    "    BAR{}: MMIO base=0x{:x} size=0x{:x} {} {}",
                    i,
                    bar.base,
                    bar.size,
                    pf,
                    bits
                );
            }
        }
    }

    if class == 0x03 && subclass == 0x00 {
        for bar in &bars {
            if bar.is_io == 0 && bar.base != 0 && bar.size != 0 {
                if state.primary_gpu.present == 0 {
                    state.primary_gpu.present = 1;
                    state.primary_gpu.device = info;
                    state.primary_gpu.mmio_phys_base = bar.base;
                    state.primary_gpu.mmio_size = bar.size;

                    let phys = PhysAddr::new(bar.base);
                    state.primary_gpu.mmio_region =
                        MmioRegion::map(phys, bar.size as usize).unwrap_or_else(MmioRegion::empty);
                    klog_info!(
                        "PCI: Selected display-class GPU candidate at MMIO phys=0x{:x} size=0x{:x} virt=0x{:x}",
                        bar.base,
                        bar.size,
                        state.primary_gpu.mmio_region.virt_base()
                    );
                }
                break;
            }
        }
    }

    if header_type == 1 {
        let secondary = pci_get_secondary_bus(bus, device, function);
        pci_scan_bus_inner(state, secondary);
    }
}

fn pci_scan_bus_inner(state: &mut PciEnumState, bus: u8) {
    if state.bus_visited[bus as usize] != 0 {
        return;
    }
    state.bus_visited[bus as usize] = 1;

    for device in 0..32u8 {
        let vendor = pci_read_vendor_id(bus, device, 0);
        if vendor == 0xFFFF {
            continue;
        }

        pci_probe_device(state, bus, device, 0);

        if pci_is_multifunction(bus, device) {
            for function in 1..8u8 {
                if pci_read_vendor_id(bus, device, function) != 0xFFFF {
                    pci_probe_device(state, bus, device, function);
                }
            }
        }
    }
}

pub fn pci_init() {
    if !PCI_INIT.init_once() {
        return;
    }

    klog_info!("PCI: Initializing PCI subsystem");

    let mut state = ENUM_STATE.lock();
    state.device_count = 0;
    state.bus_visited = [0; PCI_MAX_BUSES];
    state.primary_gpu = PciGpuInfo::zeroed();

    pci_scan_bus_inner(&mut state, 0);

    let header_type = pci_read_header_type(0, 0, 0);
    if (header_type & 0x80) != 0 {
        for function in 1..8u8 {
            if pci_read_vendor_id(0, 0, function) != 0xFFFF {
                pci_scan_bus_inner(&mut state, function);
            }
        }
    }

    let count = state.device_count;
    DEVICE_COUNT_CACHE.store(count, Ordering::Release);
    klog_info!("PCI: Enumeration complete. Devices discovered: {}", count);
}

pub fn pci_get_device_count() -> usize {
    DEVICE_COUNT_CACHE.load(Ordering::Acquire)
}

pub fn pci_get_device(index: usize) -> Option<PciDeviceInfo> {
    let state = ENUM_STATE.lock();
    if index < state.device_count {
        Some(state.devices[index])
    } else {
        None
    }
}

pub fn pci_get_primary_gpu() -> PciGpuInfo {
    ENUM_STATE.lock().primary_gpu
}

pub fn pci_register_driver(driver: &'static PciDriver) -> c_int {
    let mut registry = DRIVER_REGISTRY.lock();
    let idx = registry.count;
    if idx >= PCI_DRIVER_MAX {
        return -1;
    }
    let name = cstr_or_placeholder(driver.name);
    klog_info!("PCI: Registered driver {}", name);
    registry.drivers[idx] = driver;
    registry.count = idx + 1;
    0
}

pub fn pci_probe_drivers() {
    let registry = DRIVER_REGISTRY.lock();
    let state = ENUM_STATE.lock();

    for drv_idx in 0..registry.count {
        // SAFETY: pci_register_driver only accepts 'static PciDriver references
        let drv = unsafe { &*registry.drivers[drv_idx] };
        for dev_idx in 0..state.device_count {
            let dev = &state.devices[dev_idx];
            if let Some(mf) = drv.match_fn {
                if mf(dev, drv.context) {
                    if let Some(probe) = drv.probe {
                        let _ = probe(dev, drv.context);
                    }
                }
            }
        }
    }
}

/// Retrieve all devices that advertise MSI or MSI-X capability.
pub fn pci_get_msi_capable_devices() -> ([PciDeviceInfo; PCI_MAX_DEVICES], usize) {
    let state = ENUM_STATE.lock();
    let mut result = [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES];
    let mut count = 0;
    for i in 0..state.device_count {
        let dev = &state.devices[i];
        if dev.has_msi() || dev.has_msix() {
            result[count] = *dev;
            count += 1;
        }
    }
    (result, count)
}
