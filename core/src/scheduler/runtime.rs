use core::ffi::{c_int, c_void};
use core::ptr;

use slopos_lib::IrqMutex;
use slopos_lib::klog_info;
use spin::Once;

use super::per_cpu;
use super::scheduler::{run_ready_task_from_idle, set_scheduler_enabled, r#yield};
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_IDLE, Task, reap_zombies, task_get_info,
    task_set_current,
};
use super::work_steal::try_work_steal;

static IDLE_WAKEUP_CB: Once<IrqMutex<Option<fn() -> c_int>>> = Once::new();

pub fn scheduler_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    IDLE_WAKEUP_CB.call_once(|| IrqMutex::new(None));
    if let Some(mutex) = IDLE_WAKEUP_CB.get() {
        *mutex.lock() = callback;
    }
}

fn unified_idle_loop(_: *mut c_void) {
    loop {
        let cb = IDLE_WAKEUP_CB.get().and_then(|m| *m.lock());
        if let Some(callback) = cb {
            if callback() != 0 {
                r#yield();
                continue;
            }
        }
        let cpu_id = slopos_lib::get_current_cpu();
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

pub fn create_idle_task() -> c_int {
    create_idle_task_for_cpu(0)
}

pub fn create_idle_task_for_cpu(cpu_id: usize) -> c_int {
    let idle_task_id = unsafe {
        crate::task::task_create(
            b"idle\0".as_ptr() as *const i8,
            core::mem::transmute(unified_idle_loop as *const ()),
            ptr::null_mut(),
            TASK_PRIORITY_IDLE,
            TASK_FLAG_KERNEL_MODE,
        )
    };
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }

    unsafe {
        (*idle_task).cpu_affinity = per_cpu::affinity_mask_for_cpu(cpu_id);
        (*idle_task).last_cpu = cpu_id as u8;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleStackResolveError {
    MissingIdleTask,
    MissingKernelStack,
}

pub(crate) fn resolve_idle_stack_for_cpu(
    cpu_id: usize,
) -> Result<(*mut Task, u64), IdleStackResolveError> {
    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
    if idle_task.is_null() {
        return Err(IdleStackResolveError::MissingIdleTask);
    }

    let stack_top = unsafe { (*idle_task).kernel_stack_top };
    if stack_top == 0 {
        return Err(IdleStackResolveError::MissingKernelStack);
    }

    Ok((idle_task, stack_top))
}

#[inline(never)]
unsafe fn enter_scheduler_on_idle_stack(cpu_id: usize, idle_task: *mut Task, stack_top: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, rdx",
            "mov rbp, rsp",
            "call {target}",
            "ud2",
            target = sym scheduler_loop_entry,
            in("rdi") cpu_id,
            in("rsi") idle_task,
            in("rdx") stack_top,
            options(noreturn)
        );
    }
}

extern "C" fn scheduler_loop_entry(cpu_id: usize, idle_task: *mut Task) -> ! {
    scheduler_loop(cpu_id, idle_task)
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    let already_enabled =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.is_enabled()).unwrap_or(false);
    if already_enabled {
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    set_scheduler_enabled(true);

    slopos_lib::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let (idle_task, idle_stack_top) = match resolve_idle_stack_for_cpu(cpu_id) {
        Ok(values) => values,
        Err(IdleStackResolveError::MissingIdleTask) => {
            klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
        Err(IdleStackResolveError::MissingKernelStack) => {
            klog_info!(
                "SCHED: CPU {} idle task has no kernel stack, halting",
                cpu_id
            );
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
    };

    unsafe {
        let return_ctx = per_cpu::get_ap_return_context(cpu_id);
        if !return_ctx.is_null() {
            crate::ffi_boundary::init_kernel_context(return_ctx);
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
    });
    task_set_current(idle_task);

    unsafe { enter_scheduler_on_idle_stack(cpu_id, idle_task, idle_stack_top) }
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        if run_ready_task_from_idle(cpu_id, idle_task) {
            continue;
        }

        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        reap_zombies();

        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });

        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}
