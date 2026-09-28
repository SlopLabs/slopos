//! Shared fixtures for test-hooks-gated test modules in this crate.

use core::ffi::c_void;

use slopos_fs::vfs::{vfs_init_builtin_filesystems, vfs_open, vfs_set_mode};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

/// No-op task body; `extern "C"` to match the scheduler's `TaskEntry` alias.
pub extern "C" fn dummy_task_entry(_arg: *mut c_void) {}

pub use slopos_sched::test_fixture::mark_current_killed;

pub fn kill_task(id: u32) -> bool {
    slopos_sched::task::task_find_by_id(id)
        .is_some_and(|task| slopos_sched::task::task_kill_and_wake(&*task))
}

const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: usize = 56;
const STATIC_EXEC_HEADERS: usize = ELF64_EHDR_SIZE + ELF64_PHDR_SIZE;
const STATIC_EXEC_CODE_MAX: usize = 64;

/// Write `code` at `path` as an executable static `ET_EXEC` whose one
/// `PT_LOAD` maps the whole file, entered at `code`'s first byte.
pub fn install_static_program(path: &[u8], code: &[u8]) -> bool {
    if code.len() > STATIC_EXEC_CODE_MAX || vfs_init_builtin_filesystems().is_err() {
        return false;
    }
    let mut elf = [0u8; STATIC_EXEC_HEADERS + STATIC_EXEC_CODE_MAX];
    let len = STATIC_EXEC_HEADERS + code.len();
    let entry = PROCESS_CODE_START_VA + STATIC_EXEC_HEADERS as u64;
    let size = len as u64;
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&entry.to_le_bytes());
    elf[32..40].copy_from_slice(&(ELF64_EHDR_SIZE as u64).to_le_bytes());
    elf[52..54].copy_from_slice(&(ELF64_EHDR_SIZE as u16).to_le_bytes());
    elf[54..56].copy_from_slice(&(ELF64_PHDR_SIZE as u16).to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes());
    elf[68..72].copy_from_slice(&5u32.to_le_bytes());
    elf[80..88].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes());
    elf[88..96].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes());
    elf[96..104].copy_from_slice(&size.to_le_bytes());
    elf[104..112].copy_from_slice(&size.to_le_bytes());
    elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes());
    elf[STATIC_EXEC_HEADERS..len].copy_from_slice(code);
    let elf = &elf[..len];
    vfs_open(path, true)
        .and_then(|file| file.write(0, elf))
        .is_ok_and(|written| written == elf.len())
        && vfs_set_mode(path, 0o755).is_ok()
}
