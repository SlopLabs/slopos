//! A trap from user mode starts below the round trip that entered user mode,
//! so no depth of trap chain can reach the frames that round trip returns into.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskPriority};
use slopos_core::exec::spawn_program_with_attrs;
use slopos_fs::vfs::{vfs_init_builtin_filesystems, vfs_open, vfs_set_mode, vfs_unlink};
use slopos_kernel_services::platform::get_time_ms;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_ostd::klog_info;
use slopos_sched::ExitInfo;
use slopos_sched::scheduler::yield_;
use slopos_sched::task::{task_consume_zombie, task_find_by_id, task_peek_exit_info};
use slopos_sched::task_struct::Current;
use slopos_testing::{TestResult, assert_test};

/// Deep enough to reach the round trip's frames from `kernel_stack_top`.
const CHAIN_DEPTH: u64 = 16 * 1024;
/// Left clear below the hook for the frames of the fill itself.
const FILL_GAP: u64 = 4 * 1024;
const FILL: u64 = 0xdead_beef_dead_beef;
const EXIT_BUDGET_MS: u64 = 10_000;

const PROGRAM_PATH: &[u8] = b"/tmp/user_trap_probe";
const HEADERS: usize = 64 + 56;

/// Maps a page, writes to it so the write faults it in, and exits 0.
const CODE: [u8; 49] = [
    0xb8, 0x09, 0x00, 0x00, 0x00, // mov eax, SYS_mmap
    0x31, 0xff, // xor edi, edi
    0xbe, 0x00, 0x10, 0x00, 0x00, // mov esi, 0x1000
    0xba, 0x03, 0x00, 0x00, 0x00, // mov edx, PROT_READ | PROT_WRITE
    0x41, 0xba, 0x22, 0x00, 0x00, 0x00, // mov r10d, MAP_PRIVATE | MAP_ANONYMOUS
    0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // mov r8, -1
    0x45, 0x31, 0xc9, // xor r9d, r9d
    0x0f, 0x05, // syscall
    0xc6, 0x00, 0x01, // mov byte ptr [rax], 1
    0xb8, 0x3c, 0x00, 0x00, 0x00, // mov eax, SYS_exit
    0x31, 0xff, // xor edi, edi
    0x0f, 0x05, // syscall
    0x0f, 0x0b, // ud2
];

static ARMED: AtomicBool = AtomicBool::new(false);
static DEEP_TRAPS: AtomicU32 = AtomicU32::new(0);

/// Called on every trap from user mode that runs on the task's kernel stack.
pub(crate) fn on_user_trap() {
    if !ARMED.load(Ordering::Acquire) {
        return;
    }
    let end = slopos_ostd::cpu::x86_64::stack::read_rsp() - FILL_GAP;
    slopos_ostd::util::ptr_buf::with_buf_mut(
        (end - CHAIN_DEPTH) as *mut u64,
        (CHAIN_DEPTH / 8) as usize,
        |chain| chain.fill(FILL),
    );
    DEEP_TRAPS.fetch_add(1, Ordering::Relaxed);
}

/// A static `ET_EXEC` whose one `PT_LOAD` maps the whole file, [`CODE`] last.
fn probe_elf() -> [u8; HEADERS + CODE.len()] {
    let mut elf = [0u8; HEADERS + CODE.len()];
    let entry = PROCESS_CODE_START_VA + HEADERS as u64;
    let size = elf.len() as u64;
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&entry.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes());
    elf[68..72].copy_from_slice(&5u32.to_le_bytes());
    elf[80..88].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes());
    elf[88..96].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes());
    elf[96..104].copy_from_slice(&size.to_le_bytes());
    elf[104..112].copy_from_slice(&size.to_le_bytes());
    elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes());
    elf[HEADERS..].copy_from_slice(&CODE);
    elf
}

fn install_probe() -> bool {
    if vfs_init_builtin_filesystems().is_err() {
        return false;
    }
    let elf = probe_elf();
    vfs_open(PROGRAM_PATH, true)
        .and_then(|file| file.write(0, &elf))
        .is_ok_and(|written| written == elf.len())
        && vfs_set_mode(PROGRAM_PATH, 0o755).is_ok()
}

fn wait_for_exit(pid: u32) -> Option<ExitInfo> {
    let child = task_find_by_id(pid)?;
    let deadline = get_time_ms().saturating_add(EXIT_BUDGET_MS);
    while !child.exit_info_is_set() && get_time_ms() < deadline {
        yield_();
    }
    task_consume_zombie(pid).or_else(|| task_peek_exit_info(pid))
}

/// Every trap the probe takes from user mode fills 16 KiB of stack below
/// itself, as a deep fault or IRQ chain would; the probe must still exit 0.
pub fn test_deep_user_trap_spares_the_round_trip() -> TestResult {
    assert_test!(install_probe(), "could not write the probe program");
    // Parented here, so the probe stays a zombie until this test reaps it.
    let parent = Current::get().map_or(INVALID_TASK_ID, |current| current.id());
    DEEP_TRAPS.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
    let spawned = spawn_program_with_attrs(
        PROGRAM_PATH,
        None,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[],
        0,
        None,
        parent,
    );
    let exit = spawned.ok().and_then(wait_for_exit);
    ARMED.store(false, Ordering::Release);
    let _ = vfs_unlink(PROGRAM_PATH);

    let Some(exit) = exit else {
        klog_info!("USER_TRAP_STACK: the probe did not run to an exit");
        return TestResult::Fail;
    };
    assert_test!(
        exit.exit_code == 0,
        "the probe exited {} under deep traps",
        exit.exit_code
    );
    assert_test!(
        DEEP_TRAPS.load(Ordering::Relaxed) > 0,
        "the probe took no trap from user mode, so nothing was tested"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_deep_user_trap_spares_the_round_trip,
    suite = user_trap_stack
);

/// The outbound leg publishes the context before anything can move the task:
/// with interrupts on, a preemption between reading this CPU's PCR and writing
/// it sent the publish to the CPU the task had left, and the task running there
/// saved its next SYSCALL into this one's registers.
pub fn test_round_trip_publishes_its_context_with_interrupts_off() -> TestResult {
    const CLI: u8 = 0xfa;
    let [a, b, c, d] = (slopos_ostd::cpu::x86_64::pcr::offsets::USER_CTX_PTR as u32).to_le_bytes();
    // `mov gs:[disp32], rdi`
    let publish = [0x65, 0x48, 0x89, 0x3c, 0x25, a, b, c, d];
    let entry = slopos_ostd::user::mode::user_mode_round_trip_asm as *const u8;
    let head = slopos_ostd::util::ptr_buf::with_buf(entry, 1 + publish.len(), |code| {
        let mut head = [0u8; 10];
        head.copy_from_slice(code);
        head
    });
    assert_test!(
        head[0] == CLI,
        "the leg opens with {:#04x}, not cli",
        head[0]
    );
    assert_test!(
        head[1..] == publish,
        "the leg's first store is not the gs-relative publish: {:02x?}",
        &head[1..]
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_round_trip_publishes_its_context_with_interrupts_off,
    suite = user_trap_stack
);
