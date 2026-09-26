//! UEFI Runtime Services: `ResetSystem`, SlopOS's first-choice reboot and
//! shutdown path ahead of the ACPI fallbacks, and `GetVariable` /
//! `SetVariable`, how the booted system talks to its boot loader.
//!
//! The runtime regions are mapped only into the kernel master address space
//! (`boot::uefi_runtime`), at their identity and HHDM addresses, because
//! firmware keeps physical pointers into its own code.

use core::ffi::c_void;

/// `EFI_RESET_TYPE` (UEFI 2.x §8.5.1 ResetSystem).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EfiResetType {
    /// Cold reset: processors and devices to their initial state.
    Cold = 0,
    /// Warm reset: system-wide init, pending cycles preserved.
    Warm = 1,
    /// Power off (ACPI G2/S5 or G3).
    Shutdown = 2,
}

/// Byte offset of the `RuntimeServices` pointer within `EFI_SYSTEM_TABLE`
/// (after the 24-byte `EFI_TABLE_HEADER` and the console/vendor fields).
const SYSTEM_TABLE_RUNTIME_SERVICES: usize = 88;
/// Byte offset of `ResetSystem` within `EFI_RUNTIME_SERVICES`.
const RUNTIME_SERVICES_RESET_SYSTEM: usize = 104;
/// `EFI_TABLE_HEADER.Signature` for `EFI_RUNTIME_SERVICES` ("RUNTSERV").
const RUNTIME_SERVICES_SIGNATURE: u64 = 0x5652_4553_544e_5552;

/// Byte offset of `GetVariable` within `EFI_RUNTIME_SERVICES`.
const RUNTIME_SERVICES_GET_VARIABLE: usize = 72;
/// Byte offset of `SetVariable` within `EFI_RUNTIME_SERVICES`.
const RUNTIME_SERVICES_SET_VARIABLE: usize = 88;

/// `EFI_GUID`, in its in-memory layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EfiGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl EfiGuid {
    /// From the 16 bytes of the in-memory layout.
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self {
            data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_le_bytes([b[4], b[5]]),
            data3: u16::from_le_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }
}

/// `EFI_VARIABLE_*` attribute bits (UEFI 2.x §8.2).
pub const EFI_VARIABLE_NON_VOLATILE: u32 = 0x1;
pub const EFI_VARIABLE_BOOTSERVICE_ACCESS: u32 = 0x2;
pub const EFI_VARIABLE_RUNTIME_ACCESS: u32 = 0x4;

/// An `EFI_STATUS` error: the low bits of a status with the high bit set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EfiError {
    /// No runtime services: a BIOS boot, a malformed table, or a caller not in
    /// the kernel master address space, where alone the firmware is mapped.
    Unavailable,
    InvalidParameter,
    Unsupported,
    /// `GetVariable`'s buffer is too small; carries the size it needs.
    BufferTooSmall(usize),
    OutOfResources,
    DeviceError,
    WriteProtected,
    SecurityViolation,
    NotFound,
    Other(usize),
}

const EFI_ERROR_BIT: usize = 1 << 63;

fn efi_error(status: usize, needed: usize) -> EfiError {
    match status & !EFI_ERROR_BIT {
        2 => EfiError::InvalidParameter,
        3 => EfiError::Unsupported,
        5 => EfiError::BufferTooSmall(needed),
        7 => EfiError::DeviceError,
        8 => EfiError::WriteProtected,
        9 => EfiError::OutOfResources,
        14 => EfiError::NotFound,
        26 => EfiError::SecurityViolation,
        _ => EfiError::Other(status),
    }
}

/// `EFI_GET_VARIABLE` (UEFI 2.x §8.2.1).
type EfiGetVariableFn = unsafe extern "efiapi" fn(
    name: *const u16,
    guid: *const EfiGuid,
    attributes: *mut u32,
    data_size: *mut usize,
    data: *mut c_void,
) -> usize;

/// `EFI_SET_VARIABLE` (UEFI 2.x §8.2.3).
type EfiSetVariableFn = unsafe extern "efiapi" fn(
    name: *const u16,
    guid: *const EfiGuid,
    attributes: u32,
    data_size: usize,
    data: *const c_void,
) -> usize;

/// The runtime-services function pointer at `offset`, from a validated
/// table. `None` on anything that does not look like one.
fn runtime_service(system_table: u64, offset: usize) -> Option<u64> {
    if system_table == 0 || system_table & 0x7 != 0 {
        return None;
    }
    // SAFETY: the caller established that the firmware tables are mapped
    // (`in_firmware_space`); the offsets are spec-fixed and every pointer is
    // validated non-null, aligned and signature-matched before use.
    unsafe {
        let rs_ptr = core::ptr::read_unaligned(
            (system_table as *const u8).add(SYSTEM_TABLE_RUNTIME_SERVICES) as *const u64,
        );
        if rs_ptr == 0 || rs_ptr & 0x7 != 0 {
            return None;
        }
        if core::ptr::read_unaligned(rs_ptr as *const u64) != RUNTIME_SERVICES_SIGNATURE {
            return None;
        }
        let f = core::ptr::read_unaligned((rs_ptr as *const u8).add(offset) as *const u64);
        (f != 0).then_some(f)
    }
}

/// Whether this CPU runs on the kernel master page tables — the only address
/// space the runtime regions are mapped into.
fn in_firmware_space() -> bool {
    let Some(master) = crate::mm::vm_space::kernel_master_pml4() else {
        return false;
    };
    crate::cpu::x86_64::control_regs::read_cr3() & 0x000F_FFFF_FFFF_F000 == master.as_u64()
}

/// `GetVariable` into `data`, answering the size and attributes it holds.
///
/// Runs the firmware with interrupts masked: it is not reentrant, and firmware
/// code may use the vector registers, which a preemption part way through
/// would hand back changed. Refuses — [`EfiError::Unavailable`] — outside the
/// kernel master address space, which is also where no user task's vector
/// state lives, so only a kernel thread can succeed.
pub fn get_variable(
    system_table: u64,
    name: &[u16],
    guid: &EfiGuid,
    data: &mut [u8],
) -> Result<(usize, u32), EfiError> {
    if name.last() != Some(&0) {
        return Err(EfiError::InvalidParameter);
    }
    crate::cpu::x86_64::interrupts::IrqDisabled::with(|_| {
        if !in_firmware_space() {
            return Err(EfiError::Unavailable);
        }
        let f = runtime_service(system_table, RUNTIME_SERVICES_GET_VARIABLE)
            .ok_or(EfiError::Unavailable)?;
        let mut attributes = 0u32;
        let mut size = data.len();
        // SAFETY: `f` is the firmware's `GetVariable`, reachable because this
        // CPU is in the master address space; every argument points at live
        // kernel memory for the duration of the call, and `size` bounds the
        // write into `data`.
        let status = unsafe {
            let get: EfiGetVariableFn = core::mem::transmute(f);
            get(
                name.as_ptr(),
                guid,
                &mut attributes,
                &mut size,
                data.as_mut_ptr().cast(),
            )
        };
        if status == 0 {
            Ok((size, attributes))
        } else {
            Err(efi_error(status, size))
        }
    })
}

/// `SetVariable`; an empty `data` deletes the variable. Same context rules as
/// [`get_variable`].
pub fn set_variable(
    system_table: u64,
    name: &[u16],
    guid: &EfiGuid,
    attributes: u32,
    data: &[u8],
) -> Result<(), EfiError> {
    if name.last() != Some(&0) {
        return Err(EfiError::InvalidParameter);
    }
    crate::cpu::x86_64::interrupts::IrqDisabled::with(|_| {
        if !in_firmware_space() {
            return Err(EfiError::Unavailable);
        }
        let f = runtime_service(system_table, RUNTIME_SERVICES_SET_VARIABLE)
            .ok_or(EfiError::Unavailable)?;
        // SAFETY: as in `get_variable`; `SetVariable` only reads `data`.
        let status = unsafe {
            let set: EfiSetVariableFn = core::mem::transmute(f);
            set(
                name.as_ptr(),
                guid,
                attributes,
                data.len(),
                data.as_ptr().cast(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(efi_error(status, 0))
        }
    })
}

/// `EFI_RESET_SYSTEM` (UEFI 2.x §8.5.1).
type EfiResetSystemFn = unsafe extern "efiapi" fn(
    reset_type: u32,
    reset_status: usize,
    data_size: usize,
    reset_data: *const c_void,
);

/// Invoke `EFI_RUNTIME_SERVICES.ResetSystem` through the `EFI_SYSTEM_TABLE`
/// at virtual address `system_table`.
///
/// Returns only if the firmware ignored the request, so a fallback can run;
/// a malformed or zeroed table (including `system_table == 0` on a non-UEFI
/// boot) degrades to a no-op return.
///
/// # Preconditions
///
/// `system_table`, its `RuntimeServices` table, and the `ResetSystem` code
/// must be mapped executable in the active address space — established by
/// `boot::uefi_runtime`.
pub fn reset_system(system_table: u64, reset_type: EfiResetType) {
    if system_table == 0 || system_table & 0x7 != 0 {
        return;
    }
    // SAFETY: per the precondition the runtime regions are mapped and the
    // field offsets are spec-fixed (UEFI 2.x); every pointer is validated
    // non-null, aligned and signature-matched before it is used.
    unsafe {
        let rs_ptr = core::ptr::read_unaligned(
            (system_table as *const u8).add(SYSTEM_TABLE_RUNTIME_SERVICES) as *const u64,
        );
        if rs_ptr == 0 || rs_ptr & 0x7 != 0 {
            return;
        }
        let signature = core::ptr::read_unaligned(rs_ptr as *const u64);
        if signature != RUNTIME_SERVICES_SIGNATURE {
            return;
        }
        let reset_ptr = core::ptr::read_unaligned(
            (rs_ptr as *const u8).add(RUNTIME_SERVICES_RESET_SYSTEM) as *const u64,
        );
        if reset_ptr == 0 {
            return;
        }
        let reset: EfiResetSystemFn = core::mem::transmute(reset_ptr);
        reset(reset_type as u32, 0, 0, core::ptr::null());
    }
}
