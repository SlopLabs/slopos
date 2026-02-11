//! Syscall Validation Tests
//!
//! Targets: invalid/null pointer handling, boundary conditions,
//! permission checks, resource exhaustion, and dispatch edge cases.

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use crate::scheduler::task_struct::Task;
use crate::syscall::handlers::{syscall_arch_prctl, syscall_futex};
use crate::syscall::signal::{
    deliver_pending_signal, syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask,
    syscall_rt_sigreturn,
};
use slopos_abi::addr::PhysAddr;
use slopos_abi::signal::{
    SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGUSR1, SigSet, SignalFrame, UserSigaction, sig_bit,
};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, CLONE_SETTLS, CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, ERRNO_EAGAIN,
    FUTEX_WAIT, FUTEX_WAKE, SYSCALL_ARCH_PRCTL, SYSCALL_CLONE, SYSCALL_FUTEX, SYSCALL_KILL,
    SYSCALL_RT_SIGACTION, SYSCALL_RT_SIGPROCMASK, SYSCALL_RT_SIGRETURN,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE, TaskStatus};
use slopos_lib::InterruptFrame;
use slopos_lib::{assert_eq_test, assert_not_null, assert_test, klog_info, testing::TestResult};
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame};
use slopos_mm::paging::map_page_4kb_in_dir;
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_syscall_process_id};
use slopos_mm::user_ptr::UserPtr;

use crate::scheduler::scheduler::{init_scheduler, scheduler_shutdown};
use crate::scheduler::task::{
    init_task_manager, task_clone, task_create, task_find_by_id, task_fork, task_set_state,
    task_shutdown_all, task_terminate,
};
use crate::syscall::handlers::syscall_lookup;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

// =============================================================================
// Test Helpers
// =============================================================================

struct SyscallFixture;

impl SyscallFixture {
    fn new() -> Self {
        task_shutdown_all();
        scheduler_shutdown();
        let _ = init_task_manager();
        let _ = init_scheduler();
        Self
    }
}

impl Drop for SyscallFixture {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();
    }
}

fn dummy_task_entry(_arg: *mut c_void) {}

fn create_test_kernel_task() -> u32 {
    task_create(
        b"KernelTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_KERNEL_MODE,
    )
}

fn create_test_user_task() -> u32 {
    let user_entry = unsafe { core::mem::transmute(PROCESS_CODE_START_VA as usize) };
    task_create(
        b"UserTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    )
}

fn zero_frame() -> InterruptFrame {
    unsafe { core::mem::zeroed() }
}

struct PageDirRestoreGuard {
    prev_dir: *mut slopos_mm::paging::ProcessPageDir,
}

impl Drop for PageDirRestoreGuard {
    fn drop(&mut self) {
        if !self.prev_dir.is_null() {
            let _ = slopos_mm::paging::switch_page_directory(self.prev_dir);
        }
    }
}

fn with_user_process_context<R>(pid: u32, f: impl FnOnce() -> R) -> Option<R> {
    let page_dir = slopos_mm::process_vm::process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        return None;
    }
    let prev_dir = slopos_mm::paging::get_current_page_directory();
    if slopos_mm::paging::switch_page_directory(page_dir) != 0 {
        return None;
    }
    let _restore_guard = PageDirRestoreGuard { prev_dir };
    let _guard = set_syscall_process_id(pid);
    let out = f();
    Some(out)
}

fn user_copy_out<T: Copy>(pid: u32, addr: u64, value: &T) -> bool {
    with_user_process_context(pid, || {
        let ptr = match UserPtr::<T>::try_new(addr) {
            Ok(p) => p,
            Err(_) => return false,
        };
        copy_to_user(ptr, value).is_ok()
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(pid: u32, addr: u64) -> Option<T> {
    with_user_process_context(pid, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_page(pid: u32) -> Option<u64> {
    let base = process_vm_alloc(pid, 4096, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }

    let page_dir = slopos_mm::process_vm::process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        return None;
    }

    let phys: PhysAddr = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return None;
    }

    if map_page_4kb_in_dir(
        page_dir,
        slopos_abi::addr::VirtAddr::new(base),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        return None;
    }

    Some(base)
}

// =============================================================================
// Syscall Dispatch Tests
// =============================================================================

pub fn test_syscall_lookup_invalid_number() -> TestResult {
    assert_test!(
        syscall_lookup(0xFFFF).is_null(),
        "should reject out-of-bounds"
    );
    assert_test!(syscall_lookup(128).is_null(), "should reject boundary");
    assert_test!(syscall_lookup(u64::MAX).is_null(), "should reject u64::MAX");
    TestResult::Pass
}

pub fn test_syscall_lookup_empty_slot() -> TestResult {
    let entry = syscall_lookup(9);
    assert_test!(entry.is_null(), "unimplemented slot should return null");
    TestResult::Pass
}

pub fn test_syscall_lookup_valid() -> TestResult {
    // SYSCALL_EXIT = 1
    let entry = syscall_lookup(1);
    assert_not_null!(entry, "SYSCALL_EXIT lookup returned null");
    let entry_ref = unsafe { &*entry };
    assert_test!(entry_ref.handler.is_some(), "SYSCALL_EXIT has no handler");
    TestResult::Pass
}

pub fn test_phase56_syscall_lookup_valid() -> TestResult {
    let required = [
        SYSCALL_CLONE,
        SYSCALL_ARCH_PRCTL,
        SYSCALL_FUTEX,
        SYSCALL_RT_SIGACTION,
        SYSCALL_RT_SIGPROCMASK,
        SYSCALL_KILL,
        SYSCALL_RT_SIGRETURN,
    ];

    for sysno in required {
        let entry = syscall_lookup(sysno);
        assert_not_null!(entry, "required phase5/6 syscall missing from table");
        assert_test!(
            unsafe { (*entry).handler.is_some() },
            "required phase5/6 syscall has no handler"
        );
    }

    TestResult::Pass
}

// =============================================================================
// Fork Edge Case Tests
// =============================================================================

pub fn test_fork_null_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(ptr::null_mut());
    assert_test!(
        child_id == INVALID_TASK_ID,
        "fork with null parent should fail"
    );
    TestResult::Pass
}

pub fn test_fork_kernel_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let kernel_task_id = create_test_kernel_task();
    assert_test!(kernel_task_id != INVALID_TASK_ID);

    let kernel_task = task_find_by_id(kernel_task_id);
    assert_not_null!(kernel_task);

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(kernel_task);
    assert_test!(
        child_id == INVALID_TASK_ID,
        "kernel tasks should not be forkable"
    );

    task_terminate(kernel_task_id);
    TestResult::Pass
}

pub fn test_fork_at_task_limit() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::MAX_TASKS;

    let mut created_ids: [u32; 64] = [INVALID_TASK_ID; 64];
    let mut count = 0usize;

    for _ in 0..MAX_TASKS {
        let id = task_create(
            b"FillTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            break;
        }
        if count < created_ids.len() {
            created_ids[count] = id;
            count += 1;
        }
    }

    for i in 0..count {
        task_terminate(created_ids[i]);
    }

    TestResult::Pass
}

pub fn test_fork_terminated_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_terminate(task_id);

    let task_ptr_after = task_find_by_id(task_id);
    if !task_ptr_after.is_null() {
        let state = unsafe { (*task_ptr_after).status() };
        if state == TaskStatus::Terminated {
            let child_id = task_fork(task_ptr_after);
            assert_test!(
                child_id == INVALID_TASK_ID,
                "fork terminated parent should fail"
            );
        }
    }

    TestResult::Pass
}

pub fn test_fork_blocked_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::{task_fork, task_set_state};

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_set_state(task_id, TaskStatus::Blocked);

    let child_id = task_fork(task_ptr);

    task_terminate(task_id);
    if child_id != INVALID_TASK_ID {
        task_terminate(child_id);
    }

    TestResult::Pass
}

pub fn test_fork_cleanup_on_failure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..5 {
        let _ = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
    }

    for _ in 0..3 {
        let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);
        if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
            slopos_mm::process_vm::destroy_process_vm(child_pid);
        }
    }

    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 64, "memory leak after fork cleanup: {} pages", leak);

    TestResult::Pass
}

// =============================================================================
// Pointer Validation Tests
// =============================================================================

pub fn test_user_ptr_null() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    assert_test!(
        UserPtr::<u64>::try_new(0).is_err(),
        "null address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_kernel_address() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let kernel_addr: u64 = 0xFFFF_8000_0000_0000;
    assert_test!(
        UserPtr::<u64>::try_new(kernel_addr).is_err(),
        "kernel address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_misaligned() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let _result = UserPtr::<u64>::try_new(0x1001);
    // Just verify it doesn't crash; alignment policy is implementation-defined
    TestResult::Pass
}

pub fn test_user_ptr_overflow_boundary() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let near_max: u64 = u64::MAX - 4;
    assert_test!(
        UserPtr::<u64>::try_new(near_max).is_err(),
        "overflow-prone address should be rejected"
    );
    TestResult::Pass
}

// =============================================================================
// Syscall Argument Boundary Tests
// =============================================================================

pub fn test_brk_extreme_values() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();
    let pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(pid != slopos_abi::task::INVALID_PROCESS_ID);

    let current_brk = slopos_mm::process_vm::process_vm_brk(pid, 0);
    if current_brk == 0 {
        klog_info!("SYSCALL_TEST: Initial brk returned 0 (might be a bug)");
    }

    let max_brk = slopos_mm::process_vm::process_vm_brk(pid, u64::MAX);
    assert_test!(max_brk != u64::MAX, "brk accepted u64::MAX");

    let kernel_brk = slopos_mm::process_vm::process_vm_brk(pid, 0xFFFF_8000_0000_0000);
    assert_test!(
        kernel_brk != 0xFFFF_8000_0000_0000,
        "brk accepted kernel address"
    );

    slopos_mm::process_vm::destroy_process_vm(pid);
    TestResult::Pass
}

pub fn test_shm_create_boundaries() -> TestResult {
    let token_zero = slopos_mm::shared_memory::shm_create(1, 0, 0);
    assert_eq_test!(token_zero, 0, "shm_create accepted size 0");

    let token_one = slopos_mm::shared_memory::shm_create(1, 1, 0);
    if token_one != 0 {
        slopos_mm::shared_memory::shm_destroy(1, token_one);
    }

    let token_max = slopos_mm::shared_memory::shm_create(1, u64::MAX, 0);
    assert_eq_test!(token_max, 0, "shm_create accepted u64::MAX");

    let over_limit = (64 * 1024 * 1024) + 1;
    let token_over = slopos_mm::shared_memory::shm_create(1, over_limit, 0);
    assert_eq_test!(token_over, 0, "shm_create accepted size over limit");

    TestResult::Pass
}

// =============================================================================
// Task State Corruption Tests
// =============================================================================

pub fn test_terminate_already_terminated() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    assert_eq_test!(task_terminate(task_id), 0, "first termination failed");

    // Second termination should not crash
    let _r2 = task_terminate(task_id);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).status() };
        assert_test!(state != TaskStatus::Ready, "terminated task in READY state");
    }

    TestResult::Pass
}

pub fn test_operations_on_terminated_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);

    use crate::scheduler::task::task_get_info;
    let mut task_ptr: *mut Task = ptr::null_mut();
    let _info_result = task_get_info(task_id, &mut task_ptr);

    use crate::scheduler::task::task_set_state;
    let state_result = task_set_state(task_id, TaskStatus::Ready);
    if state_result == 0 {
        let task = task_find_by_id(task_id);
        if !task.is_null() {
            let current_state = unsafe { (*task).status() };
            assert_test!(
                current_state != TaskStatus::Ready,
                "revived terminated task"
            );
        }
    }

    TestResult::Pass
}

// =============================================================================
// Memory Pressure During Syscall Tests
// =============================================================================

pub fn test_fork_memory_pressure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..10 {
        let addr = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
        if addr == 0 {
            break;
        }
    }

    use slopos_abi::addr::PhysAddr;
    use slopos_mm::page_alloc::{ALLOC_FLAG_NO_PCP, alloc_page_frame, free_page_frame};

    let mut stress_pages: [PhysAddr; 128] = [PhysAddr::NULL; 128];
    let mut stress_count = 0usize;

    for _ in 0..128 {
        let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        stress_pages[stress_count] = phys;
        stress_count += 1;
    }

    let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
        slopos_mm::process_vm::destroy_process_vm(child_pid);
    }
    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    for i in 0..stress_count {
        free_page_frame(stress_pages[i]);
    }

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 32, "memory leak under pressure: {} pages", leak);

    TestResult::Pass
}

pub fn test_task_id_wraparound() -> TestResult {
    let _fixture = SyscallFixture::new();

    let mut ids_seen: [u32; 256] = [INVALID_TASK_ID; 256];
    let mut seen_count = 0usize;

    for _i in 0..500 {
        let id = task_create(
            b"WrapTest\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        );

        if id == INVALID_TASK_ID {
            continue;
        }

        for j in 0..seen_count {
            assert_test!(ids_seen[j] != id, "duplicate task ID {}", id);
        }

        if seen_count < ids_seen.len() {
            ids_seen[seen_count] = id;
            seen_count += 1;
        }

        task_terminate(id);
    }

    TestResult::Pass
}

pub fn test_clone_thread_tls_isolation() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    unsafe {
        (*parent_ptr).fs_base = 0x1111_2222_3333_4444;
    }

    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;
    let child_id = match task_clone(parent_ptr, flags, 0, 0, 0, 0x5555_6666_7777_8888) {
        Ok(id) => id,
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*child_ptr).tgid,
            (*parent_ptr).tgid,
            "thread did not join parent thread-group"
        );
        assert_eq_test!(
            (*child_ptr).fs_base,
            0x5555_6666_7777_8888,
            "child TLS base not set by CLONE_SETTLS"
        );
        assert_eq_test!(
            (*parent_ptr).fs_base,
            0x1111_2222_3333_4444,
            "parent TLS base unexpectedly modified"
        );
    }

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_clone_then_fork_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(parent_ptr, thread_flags, 0, 0, 0, 0) {
        Ok(id) => id,
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let fork_id = task_fork(parent_ptr);
    assert_test!(fork_id != INVALID_TASK_ID, "fork after clone failed");

    let thread_ptr = task_find_by_id(thread_id);
    let fork_ptr = task_find_by_id(fork_id);
    assert_not_null!(thread_ptr, "thread task lookup failed");
    assert_not_null!(fork_ptr, "fork child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*thread_ptr).tgid,
            (*parent_ptr).tgid,
            "thread tgid mismatch"
        );
        assert_eq_test!(
            (*fork_ptr).tgid,
            fork_id,
            "fork child should be its own thread-group leader"
        );
        assert_eq_test!(
            (*fork_ptr).parent_task_id,
            parent_id,
            "fork child parent id mismatch"
        );
    }

    task_terminate(fork_id);
    task_terminate(thread_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_futex_wait_mismatch_and_wake_no_waiters() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wait_frame = zero_frame();
    wait_frame.rdi = uaddr;
    wait_frame.rsi = FUTEX_WAIT;
    wait_frame.rdx = 2;
    wait_frame.r10 = 0;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
    assert_eq_test!(
        wait_frame.rax,
        ERRNO_EAGAIN,
        "FUTEX_WAIT mismatch must return -EAGAIN"
    );

    let mut wake_frame = zero_frame();
    wake_frame.rdi = uaddr;
    wake_frame.rsi = FUTEX_WAKE;
    wake_frame.rdx = 1;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
    assert_eq_test!(
        wake_frame.rax,
        0,
        "FUTEX_WAKE with no waiters must return 0"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_lost_wakeup_regression() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wake_frame = zero_frame();
    wake_frame.rdi = uaddr;
    wake_frame.rsi = FUTEX_WAKE;
    wake_frame.rdx = 1;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
    assert_eq_test!(
        wake_frame.rax,
        0,
        "initial FUTEX_WAKE should wake no waiters"
    );

    let mut wait_frame = zero_frame();
    wait_frame.rdi = uaddr;
    wait_frame.rsi = FUTEX_WAIT;
    wait_frame.rdx = 2;
    wait_frame.r10 = 0;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
    assert_eq_test!(
        wait_frame.rax,
        ERRNO_EAGAIN,
        "post-wake mismatch must return -EAGAIN"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_contention_path_stability() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    for i in 0..64u64 {
        let mut wake_frame = zero_frame();
        wake_frame.rdi = uaddr;
        wake_frame.rsi = FUTEX_WAKE;
        wake_frame.rdx = (i % 4) + 1;
        let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
        if wake_frame.rax > wake_frame.rdx {
            task_terminate(task_id);
            return TestResult::Fail;
        }

        let mut wait_frame = zero_frame();
        wait_frame.rdi = uaddr;
        wait_frame.rsi = FUTEX_WAIT;
        wait_frame.rdx = 2;
        wait_frame.r10 = 0;
        let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
        if wait_frame.rax != ERRNO_EAGAIN {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_install_deliver_and_sigreturn() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let new_action_addr = page;
    let old_action_addr = page + 128;

    let new_action = UserSigaction {
        sa_handler: 0x4001_0000,
        sa_flags: 0,
        sa_restorer: 0x4002_0000,
        sa_mask: sig_bit(SIGCHLD),
    };
    assert_test!(
        user_copy_out(pid, new_action_addr, &new_action),
        "failed to write new sigaction"
    );

    let mut action_frame = zero_frame();
    action_frame.rdi = SIGUSR1 as u64;
    action_frame.rsi = new_action_addr;
    action_frame.rdx = old_action_addr;
    action_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigaction(task_ptr, &mut action_frame));
    assert_eq_test!(action_frame.rax, 0, "rt_sigaction failed");

    let old_action: UserSigaction = match user_copy_in(pid, old_action_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        old_action.sa_handler,
        0,
        "initial old action should be SIG_DFL"
    );

    let stack_top = process_vm_get_stack_top(pid);
    let original_rsp = stack_top.wrapping_sub(0x200);
    let original_rip = 0x5000_1234;

    let mut kill_frame = zero_frame();
    kill_frame.rdi = task_id as u64;
    kill_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || syscall_kill(task_ptr, &mut kill_frame));
    assert_eq_test!(kill_frame.rax, 0, "kill(SIGUSR1) failed");

    let mut user_frame = zero_frame();
    user_frame.rip = original_rip;
    user_frame.rsp = original_rsp;
    user_frame.rax = 0xAA55;
    user_frame.rbx = 0xBB66;
    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));

    assert_eq_test!(
        user_frame.rip,
        new_action.sa_handler,
        "signal handler RIP not installed"
    );
    assert_eq_test!(
        user_frame.rdi,
        SIGUSR1 as u64,
        "signal number not passed in RDI"
    );

    let sigframe: SignalFrame = match user_copy_in(pid, user_frame.rsp) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        sigframe.rip,
        original_rip,
        "saved RIP mismatch in signal frame"
    );
    assert_eq_test!(
        sigframe.rsp,
        original_rsp,
        "saved RSP mismatch in signal frame"
    );
    assert_eq_test!(
        sigframe.restorer,
        new_action.sa_restorer,
        "signal restorer mismatch"
    );

    let _ = with_user_process_context(pid, || syscall_rt_sigreturn(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        original_rip,
        "rt_sigreturn did not restore RIP"
    );
    assert_eq_test!(
        user_frame.rsp,
        original_rsp,
        "rt_sigreturn did not restore RSP"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigprocmask_block_then_unblock_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let set_addr = page;
    let old_addr = page + 128;
    let act_addr = page + 256;

    let action = UserSigaction {
        sa_handler: 0x4003_0000,
        sa_flags: 0,
        sa_restorer: 0x4004_0000,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, act_addr, &action),
        "failed to write action"
    );

    let mut install_frame = zero_frame();
    install_frame.rdi = SIGUSR1 as u64;
    install_frame.rsi = act_addr;
    install_frame.rdx = 0;
    install_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigaction(task_ptr, &mut install_frame));
    assert_eq_test!(install_frame.rax, 0, "sigaction install failed");

    let block_set: SigSet = sig_bit(SIGUSR1);
    assert_test!(
        user_copy_out(pid, set_addr, &block_set),
        "failed to write block set"
    );

    let mut block_frame = zero_frame();
    block_frame.rdi = SIG_SETMASK as u64;
    block_frame.rsi = set_addr;
    block_frame.rdx = old_addr;
    block_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigprocmask(task_ptr, &mut block_frame));
    assert_eq_test!(block_frame.rax, 0, "rt_sigprocmask(SIG_SETMASK) failed");

    let mut kill_frame = zero_frame();
    kill_frame.rdi = task_id as u64;
    kill_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || syscall_kill(task_ptr, &mut kill_frame));
    assert_eq_test!(kill_frame.rax, 0, "kill(SIGUSR1) failed");

    let stack_top = process_vm_get_stack_top(pid);
    let mut user_frame = zero_frame();
    user_frame.rip = 0x6000_1111;
    user_frame.rsp = stack_top.wrapping_sub(0x200);
    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        0x6000_1111,
        "blocked signal should not be delivered"
    );

    let mut unblock_frame = zero_frame();
    unblock_frame.rdi = SIG_UNBLOCK as u64;
    unblock_frame.rsi = set_addr;
    unblock_frame.rdx = 0;
    unblock_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigprocmask(task_ptr, &mut unblock_frame));
    assert_eq_test!(unblock_frame.rax, 0, "rt_sigprocmask(SIG_UNBLOCK) failed");

    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        action.sa_handler,
        "unblocked pending signal was not delivered"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigchld_and_wait_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let child_id = task_fork(parent_ptr);
    assert_test!(child_id != INVALID_TASK_ID, "task_fork failed");

    unsafe {
        (*parent_ptr).waiting_on.store(child_id, Ordering::Release);
    }
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Running),
        0,
        "failed to set parent running"
    );
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Blocked),
        0,
        "failed to block parent"
    );

    assert_eq_test!(task_terminate(child_id), 0, "failed to terminate child");

    unsafe {
        let pending = (*parent_ptr).signal_pending.load(Ordering::Acquire);
        assert_test!(
            (pending & sig_bit(SIGCHLD)) != 0,
            "parent missing SIGCHLD pending bit"
        );
        assert_eq_test!(
            (*parent_ptr).waiting_on.load(Ordering::Acquire),
            INVALID_TASK_ID,
            "parent wait target not cleared after child exit"
        );
        assert_eq_test!(
            (*parent_ptr).status(),
            TaskStatus::Ready,
            "parent not readied after child exit"
        );
    }

    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_arch_prctl_set_get_fs_roundtrip() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let out_addr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    let expected_fs = 0x0000_4000_5678_9000u64;
    let mut set_frame = zero_frame();
    set_frame.rdi = ARCH_SET_FS;
    set_frame.rsi = expected_fs;
    let _ = with_user_process_context(pid, || syscall_arch_prctl(task_ptr, &mut set_frame));
    assert_eq_test!(set_frame.rax, 0, "ARCH_SET_FS failed");

    let mut get_frame = zero_frame();
    get_frame.rdi = ARCH_GET_FS;
    get_frame.rsi = out_addr;
    let _ = with_user_process_context(pid, || syscall_arch_prctl(task_ptr, &mut get_frame));
    assert_eq_test!(get_frame.rax, 0, "ARCH_GET_FS failed");

    let got_fs: u64 = match user_copy_in(pid, out_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(got_fs, expected_fs, "ARCH_GET_FS returned wrong value");

    let child_no_settls = match task_clone(
        task_ptr,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    ) {
        Ok(id) => id,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let child_ptr = task_find_by_id(child_no_settls);
    assert_not_null!(child_ptr, "clone child lookup failed");
    unsafe {
        assert_eq_test!(
            (*child_ptr).fs_base,
            expected_fs,
            "clone without CLONE_SETTLS must inherit FS base"
        );
    }

    task_terminate(child_no_settls);
    task_terminate(task_id);
    TestResult::Pass
}

slopos_lib::define_test_suite!(
    syscall_valid,
    [
        test_syscall_lookup_invalid_number,
        test_syscall_lookup_empty_slot,
        test_syscall_lookup_valid,
        test_phase56_syscall_lookup_valid,
        test_fork_null_parent,
        test_fork_kernel_task,
        test_fork_at_task_limit,
        test_fork_terminated_parent,
        test_fork_blocked_parent,
        test_fork_cleanup_on_failure,
        test_user_ptr_null,
        test_user_ptr_kernel_address,
        test_user_ptr_misaligned,
        test_user_ptr_overflow_boundary,
        test_brk_extreme_values,
        test_shm_create_boundaries,
        test_terminate_already_terminated,
        test_operations_on_terminated_task,
        test_fork_memory_pressure,
        test_task_id_wraparound,
        test_clone_thread_tls_isolation,
        test_clone_then_fork_interaction,
        test_futex_wait_mismatch_and_wake_no_waiters,
        test_futex_lost_wakeup_regression,
        test_futex_contention_path_stability,
        test_signal_install_deliver_and_sigreturn,
        test_sigprocmask_block_then_unblock_delivery,
        test_sigchld_and_wait_interaction,
        test_arch_prctl_set_get_fs_roundtrip,
    ]
);
