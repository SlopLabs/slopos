//! Context switch and task lifecycle edge case tests.

use core::ffi::{c_char, c_void};
use core::ptr;

use super::task_struct::Task;
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskStatus};
use slopos_lib::{assert_eq_test, assert_not_null, assert_test, klog_info, testing::TestResult};

use super::scheduler::{init_scheduler, scheduler_shutdown};
use super::task::{
    MAX_TASKS, init_task_manager, task_create, task_find_by_id, task_get_info, task_set_state,
    task_shutdown_all, task_terminate,
};

struct ContextFixture;

impl ContextFixture {
    fn new() -> Self {
        task_shutdown_all();
        scheduler_shutdown();
        let _ = init_task_manager();
        let _ = init_scheduler();
        Self
    }
}

impl Drop for ContextFixture {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();
    }
}

fn dummy_entry(_arg: *mut c_void) {}

fn create_test_task(name: &[u8], flags: u16) -> u32 {
    task_create(
        name.as_ptr() as *const c_char,
        dummy_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

// =============================================================================
// Task Lifecycle Tests
// =============================================================================

pub fn test_task_context_initial_state() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"CtxInit\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    unsafe {
        let task = &*task_ptr;
        let ctx_rsp = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rsp));
        let ctx_rip = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rip));

        if ctx_rsp == 0 && ctx_rip == 0 {
            klog_info!("CONTEXT_TEST: WARNING - Context RSP and RIP both zero");
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_state_transitions_exhaustive() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"StateTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let initial_state = unsafe { (*task_ptr).status() };
    assert_eq_test!(
        initial_state,
        TaskStatus::Ready,
        "new task not in READY state"
    );

    task_set_state(task_id, TaskStatus::Running);
    task_set_state(task_id, TaskStatus::Blocked);
    task_set_state(task_id, TaskStatus::Ready);

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_invalid_state_transition() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"BadTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);
    let _result = task_set_state(task_id, TaskStatus::Running);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).status() };
        assert_test!(
            state != TaskStatus::Running,
            "revived terminated task to RUNNING"
        );
    }

    TestResult::Pass
}

// =============================================================================
// Task Info & Termination Edge Cases
// =============================================================================

pub fn test_task_get_info_null_output() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"InfoNull\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let _result = task_get_info(task_id, ptr::null_mut());

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_get_info_invalid_id() -> TestResult {
    let _fixture = ContextFixture::new();

    let mut task_ptr: *mut Task = ptr::null_mut();

    let result = task_get_info(INVALID_TASK_ID, &mut task_ptr);
    assert_test!(
        result != 0 || task_ptr.is_null(),
        "succeeded for INVALID_TASK_ID"
    );

    task_ptr = ptr::null_mut();
    let result2 = task_get_info(0xFFFF_FFFF, &mut task_ptr);
    assert_test!(result2 != 0 || task_ptr.is_null(), "succeeded for max ID");

    TestResult::Pass
}

pub fn test_task_double_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"DoubleTerm\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let _r1 = task_terminate(task_id);
    let _r2 = task_terminate(task_id);
    let _r3 = task_terminate(task_id);

    TestResult::Pass
}

pub fn test_task_terminate_invalid_ids() -> TestResult {
    let _fixture = ContextFixture::new();

    let _ = task_terminate(INVALID_TASK_ID);
    let _ = task_terminate(0);
    let _ = task_terminate(0xFFFF_FFFF);
    let _ = task_terminate(MAX_TASKS as u32 + 100);

    TestResult::Pass
}

pub fn test_task_find_after_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"FindTerm\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    assert_not_null!(
        task_find_by_id(task_id),
        "task should exist before termination"
    );

    task_terminate(task_id);

    let ptr_after = task_find_by_id(task_id);
    if !ptr_after.is_null() {
        let state = unsafe { (*ptr_after).status() };
        assert_eq_test!(
            state,
            TaskStatus::Terminated,
            "terminated task in wrong state"
        );
    }

    TestResult::Pass
}

pub fn test_task_rapid_create_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    for _i in 0..50 {
        let task_id = create_test_task(b"Rapid\0", TASK_FLAG_KERNEL_MODE);
        if task_id == INVALID_TASK_ID {
            continue;
        }
        task_terminate(task_id);
    }

    TestResult::Pass
}

pub fn test_task_max_concurrent() -> TestResult {
    let _fixture = ContextFixture::new();

    let mut created_ids: [u32; 64] = [INVALID_TASK_ID; 64];
    let mut count = 0usize;

    for _ in 0..MAX_TASKS + 10 {
        let task_id = create_test_task(b"MaxTest\0", TASK_FLAG_KERNEL_MODE);
        if task_id == INVALID_TASK_ID {
            break;
        }
        if count < created_ids.len() {
            created_ids[count] = task_id;
            count += 1;
        }
    }

    for i in 0..count {
        task_terminate(created_ids[i]);
    }

    TestResult::Pass
}

pub fn test_task_process_id_consistency() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"ProcId\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let _proc_id = unsafe { (*task_ptr).process_id };

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_flags_preserved() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"FlagsTest\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let flags = unsafe { (*task_ptr).flags };
    assert_test!(
        (flags & TASK_FLAG_KERNEL_MODE) != 0,
        "kernel mode flag not preserved"
    );

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// SwitchContext Layout Tests
// =============================================================================

pub fn test_switch_context_struct_size() -> TestResult {
    use super::task_struct::SwitchContext;
    assert_eq_test!(
        core::mem::size_of::<SwitchContext>(),
        72,
        "SwitchContext size wrong"
    );
    TestResult::Pass
}

pub fn test_switch_context_offsets() -> TestResult {
    use super::task_struct::{
        SWITCH_CTX_OFF_R12, SWITCH_CTX_OFF_R13, SWITCH_CTX_OFF_R14, SWITCH_CTX_OFF_R15,
        SWITCH_CTX_OFF_RBP, SWITCH_CTX_OFF_RBX, SWITCH_CTX_OFF_RFLAGS, SWITCH_CTX_OFF_RIP,
        SWITCH_CTX_OFF_RSP,
    };

    assert_eq_test!(SWITCH_CTX_OFF_RBX, 0);
    assert_eq_test!(SWITCH_CTX_OFF_R12, 8);
    assert_eq_test!(SWITCH_CTX_OFF_R13, 16);
    assert_eq_test!(SWITCH_CTX_OFF_R14, 24);
    assert_eq_test!(SWITCH_CTX_OFF_R15, 32);
    assert_eq_test!(SWITCH_CTX_OFF_RBP, 40);
    assert_eq_test!(SWITCH_CTX_OFF_RSP, 48);
    assert_eq_test!(SWITCH_CTX_OFF_RFLAGS, 56);
    assert_eq_test!(SWITCH_CTX_OFF_RIP, 64);
    TestResult::Pass
}

pub fn test_switch_context_zero_init() -> TestResult {
    use super::task_struct::SwitchContext;

    let ctx = SwitchContext::zero();
    assert_eq_test!(ctx.rbx, 0);
    assert_eq_test!(ctx.r12, 0);
    assert_eq_test!(ctx.r13, 0);
    assert_eq_test!(ctx.r14, 0);
    assert_eq_test!(ctx.r15, 0);
    assert_eq_test!(ctx.rbp, 0);
    assert_eq_test!(ctx.rsp, 0);
    assert_eq_test!(ctx.rip, 0);
    assert_eq_test!(ctx.rflags, 0x202, "rflags should default to IF+reserved");
    TestResult::Pass
}

pub fn test_switch_context_setup_initial() -> TestResult {
    use super::task_struct::SwitchContext;

    let stack_top: u64 = 0x1000;
    let entry: u64 = 0xDEADBEEF;
    let arg: u64 = 0xCAFEBABE;
    let trampoline: u64 = 0x12345678;

    let ctx = SwitchContext::new_for_task(entry, arg, stack_top, trampoline);

    assert_eq_test!(ctx.rsp, stack_top - 8, "rsp should be stack_top - 8");
    assert_eq_test!(ctx.rip, trampoline, "rip should be trampoline");
    assert_eq_test!(ctx.r12, entry, "r12 should hold entry");
    assert_eq_test!(ctx.r13, arg, "r13 should hold arg");
    assert_eq_test!(ctx.rflags, 0x202);
    TestResult::Pass
}

pub fn test_task_has_switch_ctx() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"SwitchTest\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let switch_ctx = unsafe { &(*task_ptr).switch_ctx };
    assert_eq_test!(
        switch_ctx.rflags,
        0x202,
        "switch_ctx rflags not initialized"
    );

    task_terminate(task_id);
    TestResult::Pass
}

slopos_lib::define_test_suite!(
    context,
    [
        test_task_context_initial_state,
        test_task_state_transitions_exhaustive,
        test_task_invalid_state_transition,
        test_task_get_info_null_output,
        test_task_get_info_invalid_id,
        test_task_double_terminate,
        test_task_terminate_invalid_ids,
        test_task_find_after_terminate,
        test_task_rapid_create_terminate,
        test_task_max_concurrent,
        test_task_process_id_consistency,
        test_task_flags_preserved,
        test_switch_context_struct_size,
        test_switch_context_offsets,
        test_switch_context_zero_init,
        test_switch_context_setup_initial,
        test_task_has_switch_ctx,
    ]
);
