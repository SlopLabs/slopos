//! Limine bootloader handoff layer.

use core::{ffi::c_void, ptr};

use limine::{
    BaseRevision, RequestsEndMarker, RequestsStartMarker,
    file::File as LimineFile,
    memmap,
    request::{
        BootloaderInfoRequest, DateAtBootRequest, EfiMemmapRequest, EfiRequest,
        ExecutableAddressRequest, ExecutableFileRequest, FramebufferRequest, HhdmRequest,
        MemmapRequest, ModulesRequest, MpRequest, MpResponse, RsdpRequest,
    },
    uuid::Uuid,
};

use slopos_abi::DisplayInfo;
use slopos_boot_core::{Guid, layout};
use slopos_ostd::sync::{InitInPlace, OnceLock};
use slopos_ostd::{klog_debug, klog_info};

pub use slopos_ostd::boot_info::{BootFramebuffer, BootInfo, MemoryRegion, MemoryRegionKind};

slopos_ostd::limine_request! {
    start_marker,
    static LIMINE_REQUESTS_START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
}
slopos_ostd::limine_request! {
    request,
    static BASE_REVISION: BaseRevision = BaseRevision::new();
}
slopos_ostd::limine_request! {
    request,
    static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static KERNEL_FILE_REQUEST: ExecutableFileRequest = ExecutableFileRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static KERNEL_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static MP_REQUEST: MpRequest = MpRequest::new(0);
}
slopos_ostd::limine_request! {
    request,
    static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static EFI_REQUEST: EfiRequest = EfiRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static EFI_MEMMAP_REQUEST: EfiMemmapRequest = EfiMemmapRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static DATE_AT_BOOT_REQUEST: DateAtBootRequest = DateAtBootRequest::new();
}
slopos_ostd::limine_request! {
    end_marker,
    static LIMINE_REQUESTS_END_MARKER: RequestsEndMarker = RequestsEndMarker::new();
}

fn convert_entry_type(entry_type: u64) -> MemoryRegionKind {
    match entry_type {
        memmap::MEMMAP_USABLE => MemoryRegionKind::Usable,
        memmap::MEMMAP_RESERVED => MemoryRegionKind::Reserved,
        memmap::MEMMAP_ACPI_RECLAIMABLE => MemoryRegionKind::AcpiReclaimable,
        memmap::MEMMAP_ACPI_NVS => MemoryRegionKind::AcpiNvs,
        memmap::MEMMAP_BAD_MEMORY => MemoryRegionKind::BadMemory,
        memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => MemoryRegionKind::BootloaderReclaimable,
        memmap::MEMMAP_EXECUTABLE_AND_MODULES => MemoryRegionKind::KernelAndModules,
        memmap::MEMMAP_FRAMEBUFFER => MemoryRegionKind::Framebuffer,
        _ => MemoryRegionKind::Reserved,
    }
}

fn limine_entry_to_region(entry: &memmap::Entry) -> MemoryRegion {
    MemoryRegion::new(entry.base, entry.length, convert_entry_type(entry.type_))
}

#[derive(Clone, Copy)]
struct SystemFlags {
    memmap_available: bool,
    hhdm_available: bool,
    rsdp_available: bool,
}

impl SystemFlags {
    const fn new() -> Self {
        Self {
            memmap_available: false,
            hhdm_available: false,
            rsdp_available: false,
        }
    }
}

struct SystemInfo {
    framebuffer: Option<BootFramebuffer>,
    hhdm_offset: u64,
    kernel_phys_base: u64,
    kernel_virt_base: u64,
    rsdp_phys_addr: u64,
    cmdline: Option<&'static str>,
    flags: SystemFlags,
}

impl SystemInfo {
    const fn new() -> Self {
        Self {
            framebuffer: None,
            hhdm_offset: 0,
            kernel_phys_base: 0,
            kernel_virt_base: 0,
            rsdp_phys_addr: 0,
            cmdline: None,
            flags: SystemFlags::new(),
        }
    }
}

static SYSTEM_INFO: OnceLock<SystemInfo> = OnceLock::new();

fn sysinfo() -> &'static SystemInfo {
    SYSTEM_INFO
        .get()
        .expect("init_limine_protocol must run before sysinfo")
}

pub fn mp_response() -> Option<&'static MpResponse> {
    MP_REQUEST.response()
}

/// Seconds since the Unix epoch at the moment the bootloader handed off, from
/// the firmware's real-time clock. `None` when the bootloader answered nothing
/// or answered a pre-epoch value — a machine whose RTC is unset stamps no
/// timestamp rather than a negative one.
pub fn date_at_boot_unix() -> Option<u64> {
    let stamp = DATE_AT_BOOT_REQUEST.response()?.timestamp;
    u64::try_from(stamp).ok().filter(|&s| s != 0)
}

/// Appended to the loader's command line, so a kernel built for one
/// experiment carries its own options into a slot whose loader entry is fixed:
/// an option read by its last occurrence takes this value.
const BUILTIN_CMDLINE: Option<&str> = option_env!("SLOPOS_BUILTIN_CMDLINE");

const CMDLINE_MAX: usize = 1024;

/// The merged line, NUL-padded; assembled where it lies, since a buffer this
/// size would not fit a frame.
static MERGED_CMDLINE: InitInPlace<[u8; CMDLINE_MAX]> = InitInPlace::new([0; CMDLINE_MAX]);

/// The loader's command line with [`BUILTIN_CMDLINE`] after it; the loader's
/// alone when there is no built-in one or the two do not fit together.
fn merged_cmdline(loader: &'static str) -> &'static str {
    let builtin = BUILTIN_CMDLINE.map_or("", str::trim);
    let fits = loader.len() + 1 + builtin.len() < CMDLINE_MAX;
    if builtin.is_empty() || !fits {
        if !builtin.is_empty() {
            klog_info!("BOOT: built-in command line dropped: it does not fit beside the loader's");
        }
        return loader;
    }
    MERGED_CMDLINE.init_once(|bytes| {
        let mut len = 0;
        for part in [loader, builtin] {
            if part.is_empty() {
                continue;
            }
            if len != 0 {
                bytes[len] = b' ';
                len += 1;
            }
            bytes[len..len + part.len()].copy_from_slice(part.as_bytes());
            len += part.len();
        }
    });
    let Some(bytes) = MERGED_CMDLINE.get() else {
        return loader;
    };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(CMDLINE_MAX);
    let Ok(text) = core::str::from_utf8(&bytes[..len]) else {
        return loader;
    };
    klog_info!("BOOT: built-in command line appended: {}", builtin);
    text
}

fn build_system_info() -> SystemInfo {
    let mut info = SystemInfo::new();

    if let Some(resp) = BOOTLOADER_INFO_REQUEST.response() {
        let name = resp.name();
        let version = resp.version();
        klog_debug!("Bootloader: {} version {}", name, version);
    }

    if let Some(hhdm) = HHDM_REQUEST.response() {
        info.hhdm_offset = hhdm.offset;
        info.flags.hhdm_available = true;
        klog_debug!("HHDM offset: 0x{:x}", hhdm.offset);
    }

    if let Some(ka) = KERNEL_ADDRESS_REQUEST.response() {
        info.kernel_phys_base = ka.physical_base;
        info.kernel_virt_base = ka.virtual_base;
        klog_debug!(
            "Kernel phys base: 0x{:x} virt base: 0x{:x}",
            ka.physical_base,
            ka.virtual_base
        );
    }

    if let Some(rsdp) = RSDP_REQUEST.response() {
        let rsdp_ptr = rsdp.address as u64;
        info.rsdp_phys_addr = rsdp_ptr;
        info.flags.rsdp_available = rsdp_ptr != 0;

        if rsdp_ptr != 0 {
            klog_debug!("ACPI RSDP pointer: 0x{:x}", rsdp_ptr);
        } else {
            klog_info!("ACPI: Limine returned null RSDP pointer");
        }
    }

    if let Some(kf_resp) = KERNEL_FILE_REQUEST.response() {
        let kernel_file = kf_resp.executable_file();
        klog_info!(
            "BOOT: kernel {} ({} bytes), build tag {}",
            kernel_file.path(),
            kernel_file.data().len(),
            slopos_core::syscall::core_handlers::BUILD_TAG.unwrap_or("-")
        );
        let cmdline_str = merged_cmdline(kernel_file.cmdline());
        if !cmdline_str.is_empty() {
            info.cmdline = Some(cmdline_str);

            klog_debug!("Kernel cmdline: {}", cmdline_str);
        } else {
            klog_debug!("Kernel cmdline: <empty>");
        }
    }

    record_base(info.hhdm_offset);

    if let Some(memmap) = MEMMAP_REQUEST.response() {
        let entries = memmap.entries();
        let mut total = 0u64;
        let mut available = 0u64;

        for entry in entries {
            total = total.saturating_add(entry.length);
            if entry.type_ == memmap::MEMMAP_USABLE {
                available = available.saturating_add(entry.length);
            }
        }

        info.flags.memmap_available = true;

        klog_debug!(
            "Memory map: {} entries, total {} MB, available {} MB",
            entries.len(),
            total / (1024 * 1024),
            available / (1024 * 1024)
        );
    } else {
        klog_info!("WARNING: No memory map available from Limine");
    }

    if let Some(fb_resp) = FRAMEBUFFER_REQUEST.response() {
        let framebuffers = fb_resp.framebuffers();
        if let Some(fb) = framebuffers.first() {
            let display_info = DisplayInfo::from_raw(fb.width, fb.height, fb.pitch, fb.bpp);
            info.framebuffer = Some(BootFramebuffer::new(fb.address() as *mut u8, display_info));

            klog_debug!("Framebuffer: {}x{} @ {} bpp", fb.width, fb.height, fb.bpp);
            klog_debug!(
                "Framebuffer addr: 0x{:x} pitch: {}",
                fb.address() as u64,
                fb.pitch
            );
        } else {
            klog_info!("WARNING: No framebuffer provided by Limine");
        }
    } else {
        klog_info!("WARNING: No framebuffer response from Limine");
    }

    info
}

/// Whether the loader speaks the base revision this kernel asks for.
pub fn init_limine_protocol() -> bool {
    if !BASE_REVISION.is_supported() {
        klog_info!("ERROR: Limine base revision not supported!");
        return false;
    }

    SYSTEM_INFO.call_once(build_system_info);
    true
}

pub fn boot_info() -> slopos_ostd::boot_info::BootInfo {
    let info = sysinfo();
    slopos_ostd::boot_info::BootInfo {
        hhdm_offset: info.hhdm_offset,
        cmdline: info.cmdline,
        framebuffer: info.framebuffer,
        kernel_phys_base: info.kernel_phys_base,
        kernel_virt_base: info.kernel_virt_base,
        rsdp_address: info.rsdp_phys_addr,
    }
}

/// The path the loader read the kernel from, on its volume.
pub fn kernel_path() -> Option<&'static str> {
    Some(KERNEL_FILE_REQUEST.response()?.executable_file().path())
}

/// Where the protocol's `struct limine_file` keeps `gpt_disk_uuid`. The
/// `limine` crate's `File` leaves out the protocol's `unused` word after
/// `media_type`, so its UUID fields were read from four bytes earlier.
const GPT_DISK_UUID_AT: usize = 64;
const CRATE_DISK_UUID_AT: usize = core::mem::offset_of!(LimineFile, gpt_disk_uuid);
const _: () = assert!(
    core::mem::size_of::<Uuid>() == 16
        && core::mem::offset_of!(LimineFile, gpt_part_uuid) == CRATE_DISK_UUID_AT + 16
        && CRATE_DISK_UUID_AT <= GPT_DISK_UUID_AT
        && GPT_DISK_UUID_AT <= CRATE_DISK_UUID_AT + 16
);

/// The bytes `uuid` was read from. The crate's `Uuid` is not `repr(C)`, so
/// each field goes back where rustc put it.
fn raw_uuid(uuid: &Uuid) -> [u8; 16] {
    let mut raw = [0u8; 16];
    let mut put = |at: usize, bytes: &[u8]| raw[at..at + bytes.len()].copy_from_slice(bytes);
    put(core::mem::offset_of!(Uuid, a), &uuid.a.to_ne_bytes());
    put(core::mem::offset_of!(Uuid, b), &uuid.b.to_ne_bytes());
    put(core::mem::offset_of!(Uuid, c), &uuid.c.to_ne_bytes());
    put(core::mem::offset_of!(Uuid, d), &uuid.d);
    raw
}

/// The GPT disk GUID of the disk the loader read the kernel from; `None` off a
/// GPT disk.
pub fn kernel_disk_guid() -> Option<Guid> {
    let file = KERNEL_FILE_REQUEST.response()?.executable_file();
    let mut read = [0u8; 32];
    read[..16].copy_from_slice(&raw_uuid(&file.gpt_disk_uuid));
    read[16..].copy_from_slice(&raw_uuid(&file.gpt_part_uuid));
    let at = GPT_DISK_UUID_AT - CRATE_DISK_UUID_AT;
    let guid = Guid(read[at..at + 16].try_into().ok()?);
    (!guid.is_zero()).then_some(guid)
}

/// The entry's two bases, read and chosen between where the loader placed
/// them, which the HHDM maps.
fn record_base(hhdm: u64) {
    let module = |string: &str| {
        let file = module(string)?;
        let archive = file.data();
        Some(crate::base::Module {
            archive,
            at: (archive.as_ptr() as u64)
                .checked_sub(hhdm)
                .filter(|_| hhdm != 0),
        })
    };
    crate::base::record(
        module(layout::BASE_MODULE),
        module(layout::PREVIOUS_BASE_MODULE),
    );
}

fn module(string: &str) -> Option<&'static LimineFile> {
    MODULES_REQUEST
        .response()?
        .modules()
        .iter()
        .copied()
        .find(|module| module.cmdline() == string)
}

/// The install medium's archive, when the loader carried one.
pub fn install_archive() -> Option<&'static [u8]> {
    module(layout::MEDIUM_MODULE).map(LimineFile::data)
}

/// The kernel's own ELF file as the loader read it, kept for the kernel's
/// lifetime as its modules are.
pub fn kernel_file() -> Option<&'static [u8]> {
    Some(KERNEL_FILE_REQUEST.response()?.executable_file().data())
}

pub fn is_memory_map_available() -> bool {
    sysinfo().flags.memmap_available
}

pub fn get_hhdm_offset() -> u64 {
    sysinfo().hhdm_offset
}

pub fn is_hhdm_available() -> bool {
    sysinfo().flags.hhdm_available
}

pub fn kernel_cmdline_str() -> Option<&'static str> {
    sysinfo().cmdline
}

pub fn is_rsdp_available() -> bool {
    sysinfo().flags.rsdp_available
}

pub fn get_rsdp_phys_address() -> u64 {
    let info = sysinfo();
    if !info.flags.rsdp_available || info.rsdp_phys_addr == 0 {
        return 0;
    }
    // Base revision 6 returns the RSDP as an HHDM virtual pointer; older
    // revisions returned a physical address.
    let addr = info.rsdp_phys_addr;
    if info.flags.hhdm_available && addr >= info.hhdm_offset {
        addr - info.hhdm_offset
    } else {
        addr
    }
}

/// Virtual address of the `EFI_SYSTEM_TABLE`, or `0` when not booted via UEFI.
///
/// Base revision 6 returns this as an HHDM-virtual pointer; the runtime regions
/// it points into are mapped by [`crate::uefi_runtime`].
pub fn efi_system_table_addr() -> u64 {
    match EFI_REQUEST.response() {
        Some(resp) => resp.address as u64,
        None => 0,
    }
}

/// Raw UEFI memory-map blob and its per-descriptor stride (`desc_size`), or
/// `None` when not booted via UEFI. Must be consumed early in boot: the array
/// can live in bootloader-reclaimable memory.
pub fn efi_memmap() -> Option<(&'static [u8], usize)> {
    let resp = EFI_MEMMAP_REQUEST.response()?;
    Some((resp.memmap(), resp.desc_size as usize))
}

pub fn get_rsdp_address() -> *const c_void {
    let info = sysinfo();
    if !info.flags.rsdp_available || info.rsdp_phys_addr == 0 {
        return ptr::null();
    }

    let addr = info.rsdp_phys_addr;

    if addr >= info.hhdm_offset && info.flags.hhdm_available {
        addr as *const c_void
    } else if info.flags.hhdm_available {
        (addr + info.hhdm_offset) as *const c_void
    } else {
        addr as *const c_void
    }
}

pub fn memory_regions() -> impl Iterator<Item = MemoryRegion> + Clone {
    MEMMAP_REQUEST
        .response()
        .into_iter()
        .flat_map(|r| r.entries().iter())
        .map(|e| limine_entry_to_region(e))
}
