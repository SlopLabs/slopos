//! exec() ELF loader tests.

use slopos_abi::auxv::{
    AT_BASE, AT_ENTRY, AT_EXECFN, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AT_SECURE,
};
use slopos_abi::syscall::PROT_READ;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_mm::elf::{ELF_MAGIC, ElfExecInfo, ElfValidator};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm;
use slopos_ostd::klog_info;
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;

use super::{EXEC_MAX_ARG_BYTES, EXEC_MAX_ARG_STRLEN, ExecError, INIT_PATH};

static ARG_FILLER: [u8; EXEC_MAX_ARG_STRLEN] = [b'x'; EXEC_MAX_ARG_STRLEN];

const MINIMAL_ELF_SIZE: usize = 64;

fn read_user_u64(process_id: u32, addr: u64) -> Option<u64> {
    let vm_space = process_vm::process_vm_get_vm_space(resolve_pid(process_id))?;
    process_vm::process_vm_read_user_u64(&vm_space, addr)
}

fn read_user_u8(process_id: u32, addr: u64) -> Option<u8> {
    let vm_space = process_vm::process_vm_get_vm_space(resolve_pid(process_id))?;
    process_vm::process_vm_read_user_u8(&vm_space, addr)
}

fn read_user_cstr(process_id: u32, addr: u64, max_len: usize) -> Option<slopos_ostd::KVec<u8>> {
    let mut buf = slopos_ostd::KVec::<u8>::new();
    for i in 0..max_len {
        let byte = read_user_u8(process_id, addr + i as u64)?;
        if byte == 0 {
            return Some(buf);
        }
        buf.push(byte).ok()?;
    }
    Some(buf)
}

fn create_minimal_elf_header() -> [u8; MINIMAL_ELF_SIZE] {
    let mut elf = [0u8; MINIMAL_ELF_SIZE];

    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2; // EI_CLASS: 64-bit
    elf[5] = 1; // EI_DATA: little endian
    elf[6] = 1; // EI_VERSION: current
    elf[7] = 0; // EI_OSABI: SYSV
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type: ET_EXEC
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // e_machine: x86_64
    elf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    elf[24..32].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf[56..58].copy_from_slice(&0u16.to_le_bytes()); // e_phnum: 0 segments

    elf
}

fn create_elf_with_load_segment(vaddr: u64, memsz: u64, filesz: u64, offset: u64) -> [u8; 120] {
    let mut elf = [0u8; 120];

    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[7] = 0;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&vaddr.to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum: 1 segment

    elf[64..68].copy_from_slice(&1u32.to_le_bytes()); // p_type: PT_LOAD
    elf[68..72].copy_from_slice(&5u32.to_le_bytes()); // p_flags: PF_R | PF_X
    elf[72..80].copy_from_slice(&offset.to_le_bytes()); // p_offset
    elf[80..88].copy_from_slice(&vaddr.to_le_bytes()); // p_vaddr
    elf[88..96].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
    elf[96..104].copy_from_slice(&filesz.to_le_bytes()); // p_filesz
    elf[104..112].copy_from_slice(&memsz.to_le_bytes()); // p_memsz
    elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    elf
}

/// A static `ET_EXEC` whose one-page data segment sits at `data_va`, far above
/// its text: the shape of a large program's image.
fn create_exec_with_distant_data(data_va: u64) -> [u8; 176] {
    let mut elf = [0u8; 176];
    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type: ET_EXEC
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&(PROCESS_CODE_START_VA + 176).to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

    let segments = [
        (5u32, PROCESS_CODE_START_VA, 176u64, 176u64), // PF_R | PF_X, the whole file
        (6, data_va, 0, PAGE_SIZE_4KB),                // PF_R | PF_W, zero-filled
    ];
    for (index, (flags, vaddr, filesz, memsz)) in segments.iter().enumerate() {
        let at = 64 + index * 56;
        elf[at..at + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        elf[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        elf[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        elf[at + 24..at + 32].copy_from_slice(&vaddr.to_le_bytes());
        elf[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
        elf[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
        elf[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    }
    elf
}

/// An `ET_DYN` with two `PT_LOAD`s, listed in the order the caller asks for.
///
/// A real linker emits them sorted by `p_vaddr`, which is what the gABI
/// requires; the reversed order is the shape this fixture exists to feed the
/// validator.
fn create_two_segment_dyn(ascending: bool, entry: u64) -> [u8; 176] {
    let mut elf = [0u8; 176];

    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&3u16.to_le_bytes()); // e_type: ET_DYN
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&entry.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

    let low = (0x0000u64, 0x1000u64);
    let high = (0x2000u64, 0x1000u64);
    let order = if ascending { [low, high] } else { [high, low] };
    for (index, (vaddr, size)) in order.iter().enumerate() {
        let at = 64 + index * 56;
        elf[at..at + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        elf[at + 4..at + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
        elf[at + 8..at + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        elf[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        elf[at + 24..at + 32].copy_from_slice(&vaddr.to_le_bytes());
        elf[at + 32..at + 40].copy_from_slice(&0u64.to_le_bytes()); // p_filesz
        elf[at + 40..at + 48].copy_from_slice(&size.to_le_bytes()); // p_memsz
        elf[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    }
    elf
}

/// The loader's VMA pass walks the validated segments in table order, so an
/// unsorted set would leave a mapped page outside the VMA tree — invisible to
/// `exec`'s address-space reset, to the gap finder and to the page ledger.
pub fn test_elf_refuses_unsorted_load_segments() -> TestResult {
    let ascending = create_two_segment_dyn(true, 0);
    let reversed = create_two_segment_dyn(false, 0);
    let mut out = slopos_ostd::KVec::<slopos_mm::elf::ValidatedSegment>::zeroed(
        slopos_mm::elf::MAX_LOAD_SEGMENTS,
    )
    .expect("test alloc");

    let ok = ElfValidator::new(&ascending, ascending.len() as u64)
        .expect("sorted header parses")
        .validate_load_segments_into(out.as_mut_slice());
    if ok != Ok(2) {
        klog_info!(
            "EXEC_TEST: BUG - a sorted two-segment image was refused: {:?}",
            ok
        );
        return TestResult::Fail;
    }

    let bad = ElfValidator::new(&reversed, reversed.len() as u64)
        .expect("reversed header parses")
        .validate_load_segments_into(out.as_mut_slice());
    if bad.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted unsorted PT_LOAD segments");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The entry point becomes the task's user RIP, and `iretq` to a
/// non-canonical address faults in ring 0 after `swapgs`. It has to be inside
/// a loaded segment, not merely a number in the header.
pub fn test_elf_refuses_entry_outside_every_segment() -> TestResult {
    let mut out = slopos_ostd::KVec::<slopos_mm::elf::ValidatedSegment>::zeroed(
        slopos_mm::elf::MAX_LOAD_SEGMENTS,
    )
    .expect("test alloc");

    for entry in [0x0000_8000_0000_0000u64, 0x1_0000u64] {
        let elf = create_two_segment_dyn(true, entry);
        let validator = ElfValidator::new(&elf, elf.len() as u64).expect("header parses");
        let count = validator
            .validate_load_segments_into(out.as_mut_slice())
            .expect("segments validate");
        if validator
            .validate_entry_point(&out.as_slice()[..count])
            .is_ok()
        {
            klog_info!(
                "EXEC_TEST: BUG - entry {:#x} outside every segment was accepted",
                entry
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// The mapped-extent cap is per `exec`, not per image: an executable and its
/// interpreter share one budget or a dynamic `exec` spends twice what a
/// static one may.
pub fn test_segment_budget_is_shared_across_images() -> TestResult {
    let elf = create_two_segment_dyn(true, 0);
    let mut out = slopos_ostd::KVec::<slopos_mm::elf::ValidatedSegment>::zeroed(
        slopos_mm::elf::MAX_LOAD_SEGMENTS,
    )
    .expect("test alloc");

    let validator = ElfValidator::new(&elf, elf.len() as u64).expect("header parses");
    let count = validator
        .validate_load_segments_into(out.as_mut_slice())
        .expect("segments validate");
    let left = slopos_mm::elf::SegmentBudget::FULL.less(&out.as_slice()[..count]);
    if left.mapped >= slopos_mm::elf::MAX_TOTAL_MAPPED_SIZE {
        klog_info!("EXEC_TEST: BUG - the budget did not fall after an image was accepted");
        return TestResult::Fail;
    }

    let spent = slopos_mm::elf::SegmentBudget {
        mapped: 0x1000,
        zero_fill: slopos_mm::elf::MAX_TOTAL_ZERO_FILL_SIZE,
    };
    let refused = ElfValidator::new(&elf, elf.len() as u64)
        .expect("header parses")
        .with_budget(spent)
        .validate_load_segments_into(out.as_mut_slice());
    if refused.is_ok() {
        klog_info!("EXEC_TEST: BUG - an image was accepted past the remaining budget");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A large image's segments are its VMAs: its data segment can be sealed as
/// RELRO is, its heap starts above it, and the next `execve` unmaps it all.
pub fn test_a_large_image_owns_its_segments_and_the_heap_clears_it() -> TestResult {
    const DATA_VA: u64 = 0x1a0_0000;
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }
    let process = resolve_pid(pid);
    let elf = create_exec_with_distant_data(DATA_VA);
    let mut segments = slopos_ostd::KVec::<slopos_mm::elf::ValidatedSegment>::zeroed(
        slopos_mm::elf::MAX_LOAD_SEGMENTS,
    )
    .expect("test alloc");
    let mut entry = 0;
    let mapped = process_vm::process_vm_reset_for_exec(process) == 0
        && process_vm::process_vm_map_elf_image(
            process,
            &elf,
            elf.len() as u64,
            segments.as_mut_slice(),
            &mut entry,
        )
        .is_ok();
    let sealed = process_vm::process_vm_mprotect(process, DATA_VA, PAGE_SIZE_4KB, PROT_READ);
    let heap = process_vm::process_vm_brk(process, 0);
    let outlived_exec = process_vm::process_vm_reset_for_exec(process) == 0
        && process_vm::process_vm_user_va_is_user_accessible(process, DATA_VA);
    process_vm::destroy_process_vm(process);

    if !mapped {
        klog_info!("EXEC_TEST: the image with a distant data segment did not load");
        return TestResult::Fail;
    }
    if sealed != 0 {
        klog_info!(
            "EXEC_TEST: mprotect of the data segment answered {}",
            sealed
        );
        return TestResult::Fail;
    }
    if heap < DATA_VA + PAGE_SIZE_4KB {
        klog_info!(
            "EXEC_TEST: the heap starts at {:#x}, inside the image",
            heap
        );
        return TestResult::Fail;
    }
    if outlived_exec {
        klog_info!("EXEC_TEST: the data segment's page outlived the next execve");
        return TestResult::Fail;
    }
    TestResult::Pass
}

fn resolve_pid(pid: u32) -> slopos_ostd::process::ProcessId {
    slopos_ostd::process::ProcessId::resolve(pid).expect("a pid this test just created")
}

pub fn test_elf_invalid_magic() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[0] = 0x00;

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted invalid magic");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_class() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[4] = 1; // 32-bit instead of 64-bit

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted 32-bit ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_endian() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[5] = 2; // Big endian

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted big-endian ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_machine() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[18..20].copy_from_slice(&0x03u16.to_le_bytes()); // i386 instead of x86_64

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted i386 ELF on x86_64");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_truncated_header() -> TestResult {
    let elf = [0x7F, b'E', b'L', b'F', 2, 1, 1, 0];

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted truncated ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_empty_file() -> TestResult {
    let elf: [u8; 0] = [];

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted empty file");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_no_load_segments() -> TestResult {
    let elf = create_minimal_elf_header();

    let validator = match ElfValidator::new(&elf, elf.len() as u64) {
        Ok(v) => v,
        Err(_) => return TestResult::Pass,
    };

    let (_, count) = match validator.validate_load_segments() {
        Ok(segs) => segs,
        Err(_) => return TestResult::Pass,
    };

    if count > 0 {
        klog_info!("EXEC_TEST: BUG - Found segments in ELF with phnum=0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_overflow_vaddr() -> TestResult {
    let elf = create_elf_with_load_segment(
        u64::MAX - 0x1000, // vaddr near overflow
        0x2000,            // memsz that would overflow
        0x1000,
        120,
    );

    let validator = match ElfValidator::new(&elf, elf.len() as u64) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment with vaddr overflow");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_filesz_greater_than_memsz() -> TestResult {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000, // memsz
        0x2000, // filesz > memsz (invalid)
        120,
    );

    let validator = match ElfValidator::new(&elf, elf.len() as u64) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted filesz > memsz");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_offset_overflow() -> TestResult {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000,
        0x1000,
        u64::MAX, // offset that would overflow
    );

    let validator = match ElfValidator::new(&elf, elf.len() as u64) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment offset overflow");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_kernel_address_entry() -> TestResult {
    let kernel_addr: u64 = 0xFFFF_FFFF_8000_0000;
    let elf = create_elf_with_load_segment(
        kernel_addr, // vaddr in kernel space
        0x1000,      // memsz
        0x100,       // filesz
        120,         // offset (past headers)
    );

    let validator = match ElfValidator::new(&elf, elf.len() as u64) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment in kernel address space");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// One byte past the ABI path limit is `NameTooLong`, never a truncation onto
/// a shorter, existing path.
pub fn test_program_path_over_the_limit_is_refused() -> TestResult {
    let Ok(long) = slopos_ostd::KVec::filled(b'a', slopos_abi::fs::USER_PATH_MAX + 1) else {
        return TestResult::Fail;
    };
    match crate::exec::resolve_program(long.as_slice(), b"/") {
        Err(ExecError::NameTooLong) => {}
        other => {
            klog_info!(
                "EXEC_TEST: BUG - a path past the limit gave {:?}",
                other.is_ok()
            );
            return TestResult::Fail;
        }
    }
    match crate::exec::resolve_program(b"", b"/") {
        Err(ExecError::NameTooLong) => TestResult::Pass,
        other => {
            klog_info!("EXEC_TEST: BUG - an empty path gave {:?}", other.is_ok());
            TestResult::Fail
        }
    }
}

/// A relative program path resolves against the caller's working directory,
/// and the canonical answer is what the grant table keys on.
pub fn test_program_path_resolves_against_the_cwd() -> TestResult {
    const DIR: &[u8] = b"/tmp/exec_rel";
    const FULL: &[u8] = b"/tmp/exec_rel/prog";

    if slopos_fs::vfs::vfs_init_builtin_filesystems().is_err() {
        return TestResult::Skipped;
    }
    let _ = slopos_fs::vfs::vfs_mkdir(DIR);
    if slopos_fs::vfs::vfs_open(FULL, true).is_err() {
        return TestResult::Skipped;
    }

    for spelling in [b"prog".as_slice(), b"./prog".as_slice()] {
        match crate::exec::resolve_program(spelling, DIR) {
            Ok(canon) if canon.as_bytes() == FULL => {}
            Ok(canon) => {
                klog_info!(
                    "EXEC_TEST: BUG - a relative program resolved to {} bytes, not the cwd's",
                    canon.len()
                );
                return TestResult::Fail;
            }
            Err(e) => {
                klog_info!("EXEC_TEST: BUG - a relative program path failed: {:?}", e);
                return TestResult::Fail;
            }
        }
    }

    match crate::exec::resolve_program(b"prog", b"/") {
        Err(ExecError::NoEntry) => TestResult::Pass,
        other => {
            klog_info!(
                "EXEC_TEST: BUG - a relative program resolved against the root: {:?}",
                other.is_ok()
            );
            TestResult::Fail
        }
    }
}

/// Reaped, not invented: a pid out of the air is only absent by luck.
pub fn test_process_vm_root_absent_for_a_reaped_process() -> TestResult {
    let _scope = KernelTestScope::enter();

    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }
    let stale = resolve_pid(pid);
    process_vm::destroy_process_vm(stale);

    if slopos_ostd::process::ProcessId::resolve(pid).is_some() {
        klog_info!("EXEC_TEST: BUG - a reaped process's pid still resolved");
        return TestResult::Fail;
    }

    if process_vm::process_vm_get_ostd_pml4_paddr(stale) != 0 {
        klog_info!("EXEC_TEST: BUG - Got an address space for a reaped process");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_huge_segment_count() -> TestResult {
    let mut elf = create_minimal_elf_header();
    // e_phnum = 0xFFFF (maximum)
    elf[56..58].copy_from_slice(&0xFFFFu16.to_le_bytes());

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if result.is_ok() {
        let validator = result.unwrap();
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with impossible segment count");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_elf_phentsize_mismatch() -> TestResult {
    let mut elf = create_minimal_elf_header();
    // e_phentsize = 1 (way too small for a program header)
    elf[54..56].copy_from_slice(&1u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // 1 segment

    let result = ElfValidator::new(&elf, elf.len() as u64);
    if let Ok(validator) = result {
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with invalid phentsize");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_init_path_is_absolute() -> TestResult {
    if INIT_PATH.first().copied() != Some(b'/') {
        klog_info!("EXEC_TEST: BUG - INIT_PATH must be absolute");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_init_path_within_exec_limit() -> TestResult {
    if INIT_PATH.is_empty() || INIT_PATH.len() > slopos_abi::fs::USER_PATH_MAX {
        klog_info!("EXEC_TEST: BUG - INIT_PATH length invalid");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_setup_user_stack_contract_layout() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 1] = [b"/sbin/init"];
    let envs: [&[u8]; 1] = [b"TERM=slop"];
    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 3,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        interp_base: 0,
        interp_entry: 0,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let result = super::setup_user_stack(
        table,
        Some(&args),
        Some(&envs),
        &exec_info,
        b"/sbin/init",
        false,
    );
    let sp = match result {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack returned error in contract layout test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let argv0 = read_user_u64(pid, sp + 8).unwrap_or(0);
    let argv_null = read_user_u64(pid, sp + 16).unwrap_or(u64::MAX);
    let env0 = read_user_u64(pid, sp + 24).unwrap_or(0);
    let env_null = read_user_u64(pid, sp + 32).unwrap_or(u64::MAX);

    if argv0 == 0 || env0 == 0 || argv_null != 0 || env_null != 0 {
        klog_info!(
            "EXEC_TEST: stack vector layout mismatch argv0={:#x} argv_null={} env0={:#x} env_null={}",
            argv0,
            argv_null,
            env0,
            env_null
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

pub fn test_setup_user_stack_auxv_required_entries() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 2] = [b"/sbin/init", b"--smoke"];
    let envs: [&[u8]; 2] = [b"TERM=slop", b"PATH=/sbin"];
    let exec_info = ElfExecInfo {
        entry: 0x7000_1000,
        phdr_addr: 0x7000_2000,
        phent_size: 56,
        phnum: 5,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        interp_base: 0x5_0000_0000,
        interp_entry: 0x5_0000_1000,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let sp = match super::setup_user_stack(
        table,
        Some(&args),
        Some(&envs),
        &exec_info,
        b"/bin/auxv_probe",
        true,
    ) {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack returned error in auxv test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let aux_start = sp + 8 * (1 + args.len() as u64 + 1 + envs.len() as u64 + 1);
    let mut cursor = aux_start;
    let mut saw_phdr = false;
    let mut saw_phent = false;
    let mut saw_phnum = false;
    let mut saw_pagesz = false;
    let mut saw_base = false;
    let mut saw_entry = false;
    let mut saw_null = false;
    let mut secure = u64::MAX;
    let mut execfn = 0u64;

    for _ in 0..16 {
        let key = read_user_u64(pid, cursor).unwrap_or(u64::MAX);
        let val = read_user_u64(pid, cursor + 8).unwrap_or(u64::MAX);
        if key == AT_PHDR && val == exec_info.phdr_addr {
            saw_phdr = true;
        } else if key == AT_PHENT && val == exec_info.phent_size as u64 {
            saw_phent = true;
        } else if key == AT_PHNUM && val == exec_info.phnum as u64 {
            saw_phnum = true;
        } else if key == AT_PAGESZ && val == PAGE_SIZE_4KB {
            saw_pagesz = true;
        } else if key == AT_BASE && val == exec_info.interp_base {
            saw_base = true;
        } else if key == AT_ENTRY && val == exec_info.entry {
            saw_entry = true;
        } else if key == AT_SECURE {
            secure = val;
        } else if key == AT_EXECFN {
            execfn = val;
        } else if key == AT_NULL && val == 0 {
            saw_null = true;
            break;
        }
        cursor = cursor.wrapping_add(16);
    }

    let execfn_ok = execfn != 0
        && read_user_cstr(pid, execfn, 32).is_some_and(|s| s.as_slice() == b"/bin/auxv_probe");
    process_vm::destroy_process_vm(resolve_pid(pid));
    if secure != 1 || !execfn_ok {
        klog_info!(
            "EXEC_TEST: AT_SECURE={} (want 1), AT_EXECFN={:#x} names the image={}",
            secure,
            execfn,
            execfn_ok
        );
        return TestResult::Fail;
    }
    if !(saw_phdr && saw_phent && saw_phnum && saw_pagesz && saw_base && saw_entry && saw_null) {
        klog_info!(
            "EXEC_TEST: auxv missing entries phdr={} phent={} phnum={} pagesz={} base={} entry={} null={}",
            saw_phdr,
            saw_phent,
            saw_phnum,
            saw_pagesz,
            saw_base,
            saw_entry,
            saw_null
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The stack's argv *string content*, not just its pointers: the layout can
/// look correct while the strings are missing, truncated or at a wrong address.
pub fn test_setup_user_stack_argv_string_content() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 4] = [b"nc", b"-u", b"-l", b"12345"];
    let envs: [&[u8]; 1] = [b"PATH=/bin"];
    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 1,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        interp_base: 0,
        interp_entry: 0,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let sp = match super::setup_user_stack(
        table,
        Some(&args),
        Some(&envs),
        &exec_info,
        b"/sbin/init",
        false,
    ) {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack failed in argv string test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    for (i, expected) in args.iter().enumerate() {
        let ptr = match read_user_u64(pid, sp + 8 * (1 + i as u64)) {
            Some(p) if p != 0 => p,
            _ => {
                klog_info!("EXEC_TEST: argv[{}] pointer is null or unreadable", i);
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        };
        let actual = match read_user_cstr(pid, ptr, 256) {
            Some(s) => s,
            None => {
                klog_info!(
                    "EXEC_TEST: cannot read string at argv[{}] ptr={:#x}",
                    i,
                    ptr
                );
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        };
        if actual.as_slice() != *expected {
            klog_info!(
                "EXEC_TEST: argv[{}] mismatch: expected len={} got len={}",
                i,
                expected.len(),
                actual.len()
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    }

    let argv_null = read_user_u64(pid, sp + 8 * (1 + args.len() as u64)).unwrap_or(u64::MAX);
    if argv_null != 0 {
        klog_info!(
            "EXEC_TEST: argv null terminator missing, got {:#x}",
            argv_null
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let envp0_slot = sp + 8 * (1 + args.len() as u64 + 1);
    let envp0_ptr = match read_user_u64(pid, envp0_slot) {
        Some(p) if p != 0 => p,
        _ => {
            klog_info!("EXEC_TEST: envp[0] pointer is null or unreadable");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };
    let env_actual = match read_user_cstr(pid, envp0_ptr, 256) {
        Some(s) => s,
        None => {
            klog_info!(
                "EXEC_TEST: cannot read string at envp[0] ptr={:#x}",
                envp0_ptr
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };
    if env_actual.as_slice() != envs[0] {
        klog_info!(
            "EXEC_TEST: envp[0] mismatch: expected len={} got len={}",
            envs[0].len(),
            env_actual.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    // SysV ABI: sp must be 16-byte aligned at entry.
    if sp % 16 != 0 {
        klog_info!("EXEC_TEST: sp={:#x} not 16-byte aligned", sp);
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

/// The argv limit is bytes, not count: one 8-byte padding step past
/// [`EXEC_MAX_ARG_BYTES`] is refused, and landing exactly on it is accepted.
pub fn test_setup_user_stack_byte_budget_boundary() -> TestResult {
    const BUDGET: usize = EXEC_MAX_ARG_BYTES;
    const LONG_LEN: usize = EXEC_MAX_ARG_STRLEN - 1;
    const LONG_COUNT: usize = 30;
    // What 30 maximum-length strings leave, spent to the last byte by one more:
    // its cost is the padded `len + 1` plus its 8-byte pointer.
    const TAIL_COST: usize =
        BUDGET - super::EXEC_ARG_STACK_FIXED - LONG_COUNT * (EXEC_MAX_ARG_STRLEN + 8);
    const TAIL_LEN: usize = TAIL_COST - 8 - 1;

    // Heap-backed: 31 `&[u8]` slots is 496 bytes against the 2 KiB stack gate.
    let mut args = match slopos_ostd::KVec::<&[u8]>::with_capacity(LONG_COUNT + 1) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };
    for _ in 0..LONG_COUNT {
        if args.push(&ARG_FILLER[..LONG_LEN]).is_err() {
            return TestResult::Fail;
        }
    }
    if args.push(&ARG_FILLER[..TAIL_LEN + 1]).is_err() {
        return TestResult::Fail;
    }

    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }
    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    };

    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 1,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        interp_base: 0,
        interp_entry: 0,
        tls_tp: 0,
    };

    match super::setup_user_stack(
        table,
        Some(args.as_slice()),
        None,
        &exec_info,
        b"/bin/x",
        false,
    ) {
        Err(ExecError::TooManyArgs) => {}
        other => {
            klog_info!(
                "EXEC_TEST: over-budget argv not refused: ok={}",
                other.is_ok()
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    }

    args[LONG_COUNT] = &ARG_FILLER[..TAIL_LEN];
    let sp = match super::setup_user_stack(
        table,
        Some(args.as_slice()),
        None,
        &exec_info,
        b"/bin/x",
        false,
    ) {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: argv exactly at the byte budget was refused");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: at-budget argc is {}, expected {}",
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

/// Argument *count* is no longer capped: 96 arguments, three times the retired
/// limit, arrive with their strings and their order intact.
pub fn test_setup_user_stack_high_argument_count() -> TestResult {
    const HIGH_ARG_COUNT: usize = 96;

    let mut storage = match slopos_ostd::KVec::<[u8; 2]>::with_capacity(HIGH_ARG_COUNT) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };
    for i in 0..HIGH_ARG_COUNT {
        if storage
            .push([b'a' + (i / 26) as u8, b'a' + (i % 26) as u8])
            .is_err()
        {
            return TestResult::Fail;
        }
    }
    let mut args = match slopos_ostd::KVec::<&[u8]>::with_capacity(HIGH_ARG_COUNT) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };
    for entry in storage.as_slice().iter() {
        if args.push(&entry[..]).is_err() {
            return TestResult::Fail;
        }
    }

    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }
    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    };

    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 1,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        interp_base: 0,
        interp_entry: 0,
        tls_tp: 0,
    };

    let sp = match super::setup_user_stack(
        table,
        Some(args.as_slice()),
        None,
        &exec_info,
        b"/bin/x",
        false,
    ) {
        Ok(v) => v,
        Err(_) => {
            klog_info!(
                "EXEC_TEST: setup_user_stack refused {} args",
                HIGH_ARG_COUNT
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != HIGH_ARG_COUNT as u64 {
        klog_info!("EXEC_TEST: argc is {}, expected {}", argc, HIGH_ARG_COUNT);
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    for (i, expected) in args.as_slice().iter().enumerate() {
        let ptr = match read_user_u64(pid, sp + 8 * (1 + i as u64)) {
            Some(p) if p != 0 => p,
            _ => {
                klog_info!("EXEC_TEST: argv[{}] pointer is null or unreadable", i);
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        };
        match read_user_cstr(pid, ptr, 8) {
            Some(actual) if actual.as_slice() == *expected => {}
            _ => {
                klog_info!("EXEC_TEST: argv[{}] content mismatch at {:#x}", i, ptr);
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        }
    }

    let argv_null = read_user_u64(pid, sp + 8 * (1 + HIGH_ARG_COUNT as u64)).unwrap_or(u64::MAX);
    if argv_null != 0 {
        klog_info!(
            "EXEC_TEST: argv null terminator missing after {} args",
            HIGH_ARG_COUNT
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

/// A spawn file action's failure is the table's, never the spawner's memory:
/// reported as `EFAULT`, a full descriptor table read as a bad pointer.
pub fn test_spawn_fd_action_errors_are_not_faults() -> TestResult {
    use slopos_abi::Errno;
    if super::fd_action_error(Errno::EMFILE.raw()) != ExecError::TooManyFiles {
        klog_info!("EXEC_TEST: EMFILE from a file action is not reported as EMFILE");
        return TestResult::Fail;
    }
    if super::fd_action_error(Errno::ESRCH.raw()) == ExecError::Fault {
        klog_info!("EXEC_TEST: a vanished child table is reported as EFAULT");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_spawn_fd_action_errors_are_not_faults,
    suite = exec
);
slopos_testing::stest!(name = test_elf_invalid_magic, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_class, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_endian, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_machine, suite = exec);
slopos_testing::stest!(name = test_elf_truncated_header, suite = exec);
slopos_testing::stest!(name = test_elf_empty_file, suite = exec);
slopos_testing::stest!(name = test_elf_no_load_segments, suite = exec);
slopos_testing::stest!(name = test_elf_segment_overflow_vaddr, suite = exec);
slopos_testing::stest!(
    name = test_elf_segment_filesz_greater_than_memsz,
    suite = exec
);
slopos_testing::stest!(name = test_elf_segment_offset_overflow, suite = exec);
slopos_testing::stest!(name = test_elf_kernel_address_entry, suite = exec);
slopos_testing::stest!(
    name = test_program_path_over_the_limit_is_refused,
    suite = exec
);
slopos_testing::stest!(
    name = test_program_path_resolves_against_the_cwd,
    suite = exec
);
slopos_testing::stest!(
    name = test_process_vm_root_absent_for_a_reaped_process,
    suite = exec
);
slopos_testing::stest!(name = test_elf_huge_segment_count, suite = exec);
slopos_testing::stest!(name = test_elf_phentsize_mismatch, suite = exec);
slopos_testing::stest!(name = test_init_path_is_absolute, suite = exec);
slopos_testing::stest!(name = test_init_path_within_exec_limit, suite = exec);
slopos_testing::stest!(name = test_setup_user_stack_contract_layout, suite = exec);
slopos_testing::stest!(name = test_elf_refuses_unsorted_load_segments, suite = exec);
slopos_testing::stest!(
    name = test_a_large_image_owns_its_segments_and_the_heap_clears_it,
    suite = exec
);
slopos_testing::stest!(
    name = test_elf_refuses_entry_outside_every_segment,
    suite = exec
);
slopos_testing::stest!(
    name = test_segment_budget_is_shared_across_images,
    suite = exec
);
slopos_testing::stest!(
    name = test_setup_user_stack_auxv_required_entries,
    suite = exec
);
slopos_testing::stest!(
    name = test_setup_user_stack_argv_string_content,
    suite = exec
);
slopos_testing::stest!(
    name = test_setup_user_stack_byte_budget_boundary,
    suite = exec
);
slopos_testing::stest!(
    name = test_setup_user_stack_high_argument_count,
    suite = exec
);

/// The grant table is the sole source of privilege bits for a user-initiated
/// spawn. `/sbin/init` is the load-bearing negative case: granting it would let
/// any task re-spawn it and inherit console administration.
pub fn test_program_grants_are_keyed_on_exact_path() -> TestResult {
    use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TaskPriority};
    use slopos_testing::assert_test;

    use super::grants::grant_for;

    let (flags, priority) = grant_for(b"/bin/compositor");
    assert_test!(
        flags == TASK_FLAG_COMPOSITOR | slopos_abi::task::TASK_FLAG_LAUNCH,
        "the compositor must be granted COMPOSITOR and LAUNCH"
    );
    assert_test!(
        matches!(priority, Some(TaskPriority::High)),
        "the compositor must be granted the High tier the syscall refuses"
    );

    let (flags, priority) = grant_for(b"/bin/roulette");
    assert_test!(
        flags == TASK_FLAG_DISPLAY_EXCLUSIVE,
        "roulette must be granted DISPLAY_EXCLUSIVE"
    );
    assert_test!(
        priority.is_none(),
        "roulette needs no tier grant — Normal is user-requestable"
    );

    let (flags, priority) = grant_for(b"/bin/keymap");
    assert_test!(
        flags == slopos_abi::task::TASK_FLAG_CONSOLE_ADMIN,
        "the keymap program must be granted CONSOLE_ADMIN"
    );
    assert_test!(
        priority.is_none(),
        "keymap needs no tier grant — Normal is user-requestable"
    );

    assert_test!(
        grant_for(b"/bin/seat_test") == (TASK_FLAG_COMPOSITOR, None),
        "the seat test must be granted COMPOSITOR and nothing more"
    );
    assert_test!(
        grant_for(b"/bin/mount_test") == (slopos_abi::task::TASK_FLAG_MOUNT, None),
        "the mount test must be granted MOUNT and nothing more"
    );
    assert_test!(
        grant_for(b"/bin/devdisk_test") == (slopos_abi::task::TASK_FLAG_MOUNT, None),
        "the dev-disk test must be granted MOUNT and nothing more"
    );
    assert_test!(
        grant_for(b"/bin/dns_concurrent_test") == (slopos_abi::task::TASK_FLAG_NET_ADMIN, None),
        "the resolver test must be granted NET_ADMIN and nothing more"
    );

    assert_test!(
        grant_for(INIT_PATH) == (0, None),
        "init must not be grantable: SYSTEM stays kernel-only"
    );
    // The shell is a launcher, not an ordinary program: it holds `Launch` and
    // nothing else, so it may spawn a program whose identity earns authority
    // without holding any of that authority itself.
    assert_test!(
        grant_for(b"/bin/shell") == (slopos_abi::task::TASK_FLAG_LAUNCH, None),
        "the shell must be granted LAUNCH and nothing more"
    );
    assert_test!(
        grant_for(b"/bin/file_manager") == (0, None),
        "an ordinary program must get nothing"
    );
    assert_test!(
        grant_for(b"/bin/./roulette") == (0, None),
        "a non-canonical spelling must fail closed rather than be normalised"
    );
    assert_test!(
        grant_for(b"") == (0, None),
        "the empty path must get nothing"
    );

    use super::grants::covers_grant_path;
    assert_test!(
        covers_grant_path(b"/bin"),
        "/bin holds every grant path and must be uncoverable"
    );
    assert_test!(
        covers_grant_path(b"/"),
        "the root is an ancestor of every grant path"
    );
    assert_test!(
        covers_grant_path(b"/bin/halt"),
        "a grant path itself must be uncoverable"
    );
    assert_test!(
        !covers_grant_path(b"/bindings"),
        "a prefix that is not a component boundary must not match"
    );
    assert_test!(
        !covers_grant_path(b"/bin/halt/deeper"),
        "a path below a grant path covers nothing: no grant is keyed under it"
    );
    assert_test!(
        !covers_grant_path(b"/tmp"),
        "an ordinary directory must stay mountable"
    );
    assert_test!(
        covers_grant_path(b"/lib"),
        "the interpreter runs before the program, so its directory is uncoverable"
    );

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_program_grants_are_keyed_on_exact_path,
    suite = exec
);
