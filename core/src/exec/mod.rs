//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

pub mod tests;

use alloc::vec::Vec;
use core::ffi::c_char;
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_USER_MODE};
use slopos_fs::vfs::ops::vfs_open;
use slopos_lib::klog_info;
use slopos_mm::elf::ElfError;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mm_constants::{PAGE_SIZE_4KB, PROCESS_CODE_START_VA};
use slopos_mm::process_vm::{
    process_vm_get_page_dir, process_vm_get_stack_top, process_vm_load_elf_data,
    process_vm_translate_elf_address,
};

use crate::{
    INVALID_TASK_ID, Task, TaskEntry, schedule_task, task_create, task_get_info, task_terminate,
};

extern crate alloc;

pub const EXEC_MAX_PATH: usize = 256;
pub const EXEC_MAX_ARG_STRLEN: usize = 4096;
pub const EXEC_MAX_ARGS: usize = 32;
pub const EXEC_MAX_ENVS: usize = 32;
pub const EXEC_MAX_ELF_SIZE: usize = 16 * 1024 * 1024;

pub const INIT_PATH: &[u8] = b"/sbin/init";

#[derive(Clone, Copy, Debug)]
pub struct ProgramSpec {
    pub task_name: &'static [u8],
    pub path: &'static [u8],
    pub priority: u8,
    pub flags: u16,
}

const PROGRAM_TABLE: [ProgramSpec; 6] = [
    ProgramSpec {
        task_name: b"init\0",
        path: b"/sbin/init",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        task_name: b"shell\0",
        path: b"/bin/shell",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        task_name: b"compositor\0",
        path: b"/bin/compositor",
        priority: 4,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR,
    },
    ProgramSpec {
        task_name: b"roulette\0",
        path: b"/bin/roulette",
        priority: 5,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_DISPLAY_EXCLUSIVE,
    },
    ProgramSpec {
        task_name: b"file_manager\0",
        path: b"/bin/file_manager",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        task_name: b"sysinfo\0",
        path: b"/bin/sysinfo",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExecError {
    NoEntry = -2,
    NoExec = -8,
    NoMem = -12,
    Fault = -14,
    NameTooLong = -36,
    IoError = -5,
    TooManyArgs = -7,
}

impl From<ElfError> for ExecError {
    fn from(_: ElfError) -> Self {
        ExecError::NoExec
    }
}

fn trim_nul_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..len]
}

pub fn resolve_program_spec(name: &[u8]) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(name);
    PROGRAM_TABLE
        .iter()
        .find(|spec| trim_nul_bytes(spec.task_name) == requested)
}

pub fn launch_init() -> Result<u32, ExecError> {
    spawn_program(&PROGRAM_TABLE[0])
}

pub fn spawn_program_by_name(name: &[u8]) -> Result<u32, ExecError> {
    let spec = resolve_program_spec(name).ok_or(ExecError::NoEntry)?;
    spawn_program(spec)
}

pub fn spawn_program(spec: &ProgramSpec) -> Result<u32, ExecError> {
    let user_code_entry: TaskEntry =
        unsafe { core::mem::transmute(PROCESS_CODE_START_VA as usize) };

    let task_id = task_create(
        spec.task_name.as_ptr() as *const c_char,
        user_code_entry,
        ptr::null_mut(),
        spec.priority,
        spec.flags,
    );

    if task_id == INVALID_TASK_ID {
        return Err(ExecError::NoMem);
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_info) != 0 || task_info.is_null() {
        task_terminate(task_id);
        return Err(ExecError::Fault);
    }

    let process_id = unsafe { (*task_info).process_id };
    let mut entry = 0u64;
    let mut stack_ptr = 0u64;

    if let Err(err) = do_exec(
        process_id,
        spec.path,
        None,
        None,
        &mut entry,
        &mut stack_ptr,
    ) {
        task_terminate(task_id);
        return Err(err);
    }

    unsafe {
        (*task_info).entry_point = entry;
        ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rip), entry);
        ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rsp), stack_ptr);
    }

    if schedule_task(task_info) != 0 {
        task_terminate(task_id);
        return Err(ExecError::NoMem);
    }

    Ok(task_id)
}

pub fn do_exec(
    process_id: u32,
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    entry_out: &mut u64,
    stack_ptr_out: &mut u64,
) -> Result<(), ExecError> {
    if path.is_empty() || path.len() > EXEC_MAX_PATH {
        return Err(ExecError::NameTooLong);
    }

    let handle = vfs_open(path, false).map_err(|e| match e {
        slopos_fs::VfsError::NotFound => ExecError::NoEntry,
        slopos_fs::VfsError::IsDirectory => ExecError::NoExec,
        slopos_fs::VfsError::PermissionDenied => ExecError::NoExec,
        _ => ExecError::IoError,
    })?;

    let file_stat = handle
        .fs
        .stat(handle.inode)
        .map_err(|_| ExecError::IoError)?;
    if (file_stat.mode & 0o111) == 0 {
        return Err(ExecError::NoExec);
    }

    let file_size = file_stat.size as usize;
    if file_size == 0 || file_size > EXEC_MAX_ELF_SIZE {
        return Err(ExecError::NoExec);
    }

    let mut elf_data: Vec<u8> = Vec::new();
    elf_data
        .try_reserve(file_size)
        .map_err(|_| ExecError::NoMem)?;
    elf_data.resize(file_size, 0);

    let mut offset = 0u64;
    while (offset as usize) < file_size {
        let remaining = file_size - offset as usize;
        let chunk_size = remaining.min(4096);
        let read = handle
            .read(
                offset,
                &mut elf_data[offset as usize..offset as usize + chunk_size],
            )
            .map_err(|_| ExecError::IoError)?;
        if read == 0 {
            break;
        }
        offset += read as u64;
    }

    if (offset as usize) < file_size {
        elf_data.truncate(offset as usize);
    }

    process_vm_load_elf_data(process_id, &elf_data, entry_out).map_err(ExecError::from)?;

    let stack_top = setup_user_stack(process_id, argv, envp)?;
    *stack_ptr_out = stack_top;

    klog_info!(
        "exec: loaded ELF for process {}, entry={:#x}, stack={:#x}",
        process_id,
        *entry_out,
        stack_top
    );

    Ok(())
}

pub fn translate_address(addr: u64, min_vaddr: u64, code_base: u64) -> u64 {
    process_vm_translate_elf_address(addr, min_vaddr, code_base)
}

fn setup_user_stack(
    process_id: u32,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
) -> Result<u64, ExecError> {
    let stack_top_raw = process_vm_get_stack_top(process_id);
    if stack_top_raw == 0 {
        return Err(ExecError::Fault);
    }
    let stack_top = stack_top_raw.wrapping_sub(8);

    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        return Err(ExecError::NoMem);
    }

    let argc = argv.map(|a| a.len()).unwrap_or(0);
    let envc = envp.map(|e| e.len()).unwrap_or(0);

    if argc > EXEC_MAX_ARGS || envc > EXEC_MAX_ENVS {
        return Err(ExecError::TooManyArgs);
    }

    let mut sp = stack_top;
    sp = sp.wrapping_sub(128);
    sp &= !0xF;

    let mut string_ptrs: Vec<u64> = Vec::new();
    string_ptrs
        .try_reserve(argc + envc + 2)
        .map_err(|_| ExecError::NoMem)?;

    if let Some(args) = argv {
        for arg in args.iter() {
            let len = arg.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(page_dir, sp, arg)?;
            write_byte_to_user_stack(page_dir, sp + arg.len() as u64, 0)?;
            string_ptrs.push(sp);
        }
    }

    let argv_start = string_ptrs.len();

    if let Some(envs) = envp {
        for env in envs.iter() {
            let len = env.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(page_dir, sp, env)?;
            write_byte_to_user_stack(page_dir, sp + env.len() as u64, 0)?;
            string_ptrs.push(sp);
        }
    }

    sp &= !0xF;

    let aux_size = 2 * 8;
    sp = sp.wrapping_sub(aux_size);
    write_u64_to_user_stack(page_dir, sp, 0)?;
    write_u64_to_user_stack(page_dir, sp + 8, 0)?;

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, 0)?;

    for i in (argv_start..string_ptrs.len()).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(page_dir, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, 0)?;

    for i in (0..argv_start).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(page_dir, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, argc as u64)?;

    sp &= !0xF;
    if ((stack_top - sp) / 8) % 2 != 0 {
        sp = sp.wrapping_sub(8);
    }

    Ok(sp)
}

fn write_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    data: &[u8],
) -> Result<(), ExecError> {
    use slopos_mm::paging::virt_to_phys_in_dir;

    for (i, &byte) in data.iter().enumerate() {
        let va = addr + i as u64;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;

        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ExecError::Fault);
        }
        let virt = phys.to_virt();
        if virt.is_null() {
            return Err(ExecError::Fault);
        }
        unsafe {
            *virt.as_mut_ptr::<u8>().add(page_off) = byte;
        }
    }
    Ok(())
}

fn write_byte_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    byte: u8,
) -> Result<(), ExecError> {
    write_to_user_stack(page_dir, addr, &[byte])
}

fn write_u64_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    value: u64,
) -> Result<(), ExecError> {
    let bytes = value.to_le_bytes();
    write_to_user_stack(page_dir, addr, &bytes)
}
