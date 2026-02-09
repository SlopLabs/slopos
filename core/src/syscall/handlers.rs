use core::ffi::c_char;
use core::ptr;

use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_abi::fate::FateResult;
use slopos_abi::syscall::*;
use slopos_abi::DisplayInfo;
use slopos_abi::InputEvent;
use slopos_abi::WindowInfo;

use crate::exec;

use crate::fate_api::{fate_apply_outcome, fate_set_pending, fate_spin, fate_take_pending};
use crate::platform;
use crate::sched::{
    clear_scheduler_current_task, get_scheduler_stats, schedule, scheduler_is_preemption_enabled,
    sleep_current_task_ms, task_wait_for, yield_,
};
use crate::syscall::common::{
    syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_copy_user_str,
    syscall_return_err, SyscallDisposition, SyscallEntry, USER_IO_MAX_BYTES,
};
use crate::syscall::context::SyscallContext;
use crate::syscall::fs::{
    syscall_fs_close, syscall_fs_list, syscall_fs_mkdir, syscall_fs_open, syscall_fs_read,
    syscall_fs_stat, syscall_fs_unlink, syscall_fs_write,
};
use crate::syscall_services::{input, tty, video};
use crate::task::{get_task_stats, task_get_exit_record, task_terminate};

use crate::scheduler::task_struct::Task;
use slopos_abi::task::{TaskExitReason, TaskExitRecord, TaskFaultReason, INVALID_TASK_ID};
use slopos_lib::klog_debug;
use slopos_lib::wl_currency;
use slopos_lib::InterruptFrame;
use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::paging;
use slopos_mm::user_copy::{copy_bytes_from_user, copy_to_user};
use slopos_mm::user_ptr::UserBytes;
use slopos_mm::user_ptr::UserPtr;

pub fn syscall_yield(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let _ = ctx.ok(0);
    yield_();
    SyscallDisposition::Ok
}

define_syscall!(syscall_random_next(ctx, args) {
    let value = platform::rng_next();
    ctx.ok(value)
});

define_syscall!(syscall_get_time_ms(ctx, args) {
    let ms = platform::get_time_ms();
    ctx.ok(ms)
});

define_syscall!(syscall_shm_get_formats(ctx, args) {
    let formats = slopos_mm::shared_memory::shm_get_formats();
    ctx.ok(formats as u64)
});

pub fn syscall_halt(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    platform::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

pub fn syscall_reboot(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    wl_currency::award_win();
    platform::kernel_reboot(b"user reboot\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

define_syscall!(syscall_sleep_ms(ctx, args) {
    let mut ms = args.arg0;
    if ms > 60000 {
        ms = 60000;
    }
    let rc = if scheduler_is_preemption_enabled() != 0 {
        sleep_current_task_ms(ms as u32)
    } else {
        crate::platform::timer_poll_delay_ms(ms as u32);
        0
    };
    if rc == 0 {
        ctx.ok(0)
    } else {
        ctx.err()
    }
});

pub fn syscall_exit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let ctx = SyscallContext::new(task, frame);
    let task_id = ctx.as_ref().and_then(|c| c.task_id()).unwrap_or(u32::MAX);
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    if let Some(ref c) = ctx {
        if let Some(t) = c.task_mut() {
            t.exit_reason = TaskExitReason::Normal;
            t.fault_reason = TaskFaultReason::None;
            t.exit_code = 0;
        }
    }
    klog_debug!("SYSCALL_EXIT: task {} calling task_terminate", task_id);
    task_terminate(task_id);
    clear_scheduler_current_task();
    schedule();
    klog_debug!(
        "SYSCALL_EXIT: task {} schedule returned (should not happen)",
        task_id
    );
    SyscallDisposition::NoReturn
}

define_syscall!(syscall_surface_commit(ctx, args) requires(let task_id) {
    ctx.from_result(video::surface_commit(task_id))
});

define_syscall!(syscall_surface_frame(ctx, args) requires(let task_id) {
    ctx.from_result(video::surface_request_frame_callback(task_id))
});

define_syscall!(syscall_poll_frame_done(ctx, args) requires(let task_id) {
    let timestamp = video::surface_poll_frame_done(task_id);
    ctx.ok(timestamp)
});

define_syscall!(syscall_buffer_age(ctx, args) requires(let task_id) {
    let age = video::surface_get_buffer_age(task_id);
    ctx.ok(age as u64)
});

define_syscall!(syscall_shm_poll_released(ctx, args) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_poll_released(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_surface_damage(ctx, args) requires(let task_id) {
    let x = args.arg0_i32();
    let y = args.arg1_i32();
    let width = args.arg2_i32();
    let height = args.arg3_i32();
    ctx.from_result(video::surface_add_damage(task_id, x, y, width, height))
});

define_syscall!(syscall_shm_create(ctx, args) requires(let process_id) {
    let size = args.arg0;
    let flags = args.arg1_u32();
    ctx.from_token(slopos_mm::shared_memory::shm_create(process_id, size, flags))
});

define_syscall!(syscall_shm_map(ctx, args) requires(let process_id) {
    let token = args.arg0_u32();
    let access_val = args.arg1_u32();
    let access = some_or_err!(ctx, slopos_mm::shared_memory::ShmAccess::from_u32(access_val));
    ctx.from_nonzero(slopos_mm::shared_memory::shm_map(process_id, token, access))
});

define_syscall!(syscall_shm_unmap(ctx, args) requires(let process_id) {
    let vaddr = args.arg0;
    let result = slopos_mm::shared_memory::shm_unmap(process_id, vaddr);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_shm_destroy(ctx, args) requires(let process_id) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_destroy(process_id, token);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_surface_attach(ctx, args) requires(let task_id, let process_id) {
    let token = args.arg0_u32();
    let width = args.arg1_u32();
    let height = args.arg2_u32();
    let result = slopos_mm::shared_memory::surface_attach(process_id, token, width, height);
    check_result!(ctx, result);
    if video::register_surface(task_id, width, height, token).is_err() {
        return ctx.err();
    }
    ctx.ok(0)
});

define_syscall!(syscall_shm_create_with_format(ctx, args) requires(let task_id) {
    let size = args.arg0;
    let format_val = args.arg1_u32();
    let format = some_or_err!(ctx, slopos_mm::shared_memory::PixelFormat::from_u32(format_val));
    ctx.from_token(slopos_mm::shared_memory::shm_create_with_format(task_id, size, format))
});

define_syscall!(syscall_surface_set_role(ctx, args) requires(let task_id) {
    let role = args.arg0 as u8;
    ctx.from_result(video::surface_set_role(task_id, role))
});

define_syscall!(syscall_surface_set_parent(ctx, args) requires(let task_id) {
    let parent_task_id = args.arg0_u32();
    ctx.from_result(video::surface_set_parent(task_id, parent_task_id))
});

define_syscall!(syscall_surface_set_rel_pos(ctx, args) requires(let task_id) {
    let rel_x = args.arg0_i32();
    let rel_y = args.arg1_i32();
    ctx.from_result(video::surface_set_relative_position(task_id, rel_x, rel_y))
});

define_syscall!(syscall_surface_set_title(ctx, args) requires(let task_id) {
    let title_ptr = args.arg0_const_ptr::<u8>();
    let title_len = args.arg1_usize();

    if title_ptr.is_null() || title_len == 0 {
        return ctx.err();
    }

    let copy_len = title_len.min(31);
    let title_slice = unsafe { core::slice::from_raw_parts(title_ptr, copy_len) };
    ctx.from_result(video::surface_set_title(task_id, title_slice))
});

define_syscall!(syscall_input_poll(ctx, args) requires(let task_id) {
    let event_ptr = args.arg0_ptr::<InputEvent>();
    if event_ptr.is_null() {
        return ctx.ok((-1i64) as u64);
    }

    if ctx.is_compositor() && input::get_pointer_focus() == 0 {
        input::set_pointer_focus(task_id, 0);
    }

    match input::poll(task_id) {
        Some(event) => {
            unsafe { *event_ptr = event; }
            ctx.ok(1)
        }
        None => ctx.ok(0),
    }
});

define_syscall!(syscall_input_poll_batch(ctx, args) requires(let task_id) {
    let buffer_ptr = args.arg0_ptr::<InputEvent>();
    let max_count = args.arg1_usize();

    if buffer_ptr.is_null() || max_count == 0 {
        return ctx.ok(0);
    }

    if ctx.is_compositor() && input::get_pointer_focus() == 0 {
        input::set_pointer_focus(task_id, 0);
    }

    ctx.ok(input::drain_batch(task_id, buffer_ptr, max_count) as u64)
});

define_syscall!(syscall_input_has_events(ctx, args) requires(let task_id) {
    let count = input::event_count(task_id);
    ctx.ok(count as u64)
});

define_syscall!(syscall_input_set_focus(ctx, args) requires(let task_id) {
    let _ = task_id;
    let target_task_id = args.arg0_u32();
    let focus_type = args.arg1_u32();
    let timestamp_ms = platform::get_time_ms();

    match focus_type {
        0 => input::set_keyboard_focus(target_task_id),
        1 => input::set_pointer_focus(target_task_id, timestamp_ms),
        _ => return ctx.ok((-1i64) as u64),
    }
    ctx.ok(0)
});

define_syscall!(syscall_input_set_focus_with_offset(ctx, args) requires(compositor) {
    let target_task_id = args.arg0_u32();
    let offset_x = args.arg1 as i32;
    let offset_y = args.arg2 as i32;
    let timestamp_ms = platform::get_time_ms();
    input::set_pointer_focus_with_offset(target_task_id, offset_x, offset_y, timestamp_ms);
    ctx.ok(0)
});

define_syscall!(syscall_input_get_pointer_pos(ctx, args) requires(compositor) {
    let (x, y) = input::get_pointer_position();
    let result = ((x as u32 as u64) << 32) | (y as u32 as u64);
    ctx.ok(result)
});

define_syscall!(syscall_input_get_button_state(ctx, args) requires(compositor) {
    let buttons = input::get_button_state();
    ctx.ok(buttons as u64)
});

define_syscall!(syscall_input_request_close(ctx, args) requires(compositor) {
    let target_task_id = args.arg0_u32();
    if target_task_id == 0 || target_task_id == INVALID_TASK_ID {
        wl_currency::award_loss();
        return ctx.err();
    }

    let timestamp_ms = platform::get_time_ms();
    if input::request_close(target_task_id, timestamp_ms) != 0 {
        wl_currency::award_loss();
        return ctx.err();
    }

    wl_currency::award_win();
    ctx.ok(0)
});

define_syscall!(syscall_tty_set_focus(ctx, args) requires(compositor) {
    let target = args.arg0_u32();
    ctx.from_bool_value(tty::set_focus(target) == 0, tty::get_focus() as u64)
});

define_syscall!(syscall_enumerate_windows(ctx, args) requires(compositor) {
    let out_buffer = args.arg0_ptr::<WindowInfo>();
    let max_count = args.arg1_u32();
    require_nonnull!(ctx, out_buffer);
    require_nonzero!(ctx, max_count);
    ctx.ok(video::surface_enumerate_windows(out_buffer, max_count) as u64)
});

define_syscall!(syscall_set_window_position(ctx, args) requires(compositor) {
    let target_task_id = args.arg0_u32();
    let x = args.arg1_i32();
    let y = args.arg2_i32();
    ctx.from_result(video::surface_set_window_position(target_task_id, x, y))
});

define_syscall!(syscall_set_window_state(ctx, args) requires(compositor) {
    let target_task_id = args.arg0_u32();
    let state = args.arg1 as u8;
    ctx.from_result(video::surface_set_window_state(target_task_id, state))
});

define_syscall!(syscall_raise_window(ctx, args) requires(compositor) {
    let target_task_id = args.arg0_u32();
    ctx.from_result(video::surface_raise_window(target_task_id))
});

define_syscall!(syscall_fb_flip(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let damage_ptr = args.arg1;
    let damage_count = args.arg2_usize();
    let phys_addr = slopos_mm::shared_memory::shm_get_phys_addr(token);
    let size = slopos_mm::shared_memory::shm_get_size(token);
    if phys_addr.is_null() || size == 0 {
        return ctx.err();
    }

    let mut damage_regions = [DamageRect::invalid(); MAX_DAMAGE_REGIONS];
    let mut damage_region_count = 0u32;
    if damage_ptr != 0 && damage_count > 0 {
        let clamped = damage_count.min(MAX_DAMAGE_REGIONS);
        let byte_len = core::mem::size_of::<DamageRect>() * clamped;
        let user_bytes = match UserBytes::try_new(damage_ptr, byte_len) {
            Ok(ptr) => ptr,
            Err(_) => return ctx.err(),
        };
        let dst = &mut damage_regions[..clamped];
        let dst_bytes = unsafe {
            core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, byte_len)
        };
        if copy_bytes_from_user(user_bytes, dst_bytes).is_err() {
            return ctx.err();
        }
        damage_region_count = clamped as u32;
    }

    some_or_err!(ctx, video::get_display_info());
    let damage_ptr = if damage_region_count > 0 {
        damage_regions.as_ptr()
    } else {
        core::ptr::null()
    };
    check_result!(ctx, video::fb_flip_from_shm(
        phys_addr,
        size,
        damage_ptr,
        damage_region_count,
    ));
    ctx.ok(0)
});

define_syscall!(syscall_drain_queue(ctx, args) requires(compositor) {
    video::drain_queue();
    ctx.ok(0)
});

define_syscall!(syscall_shm_acquire(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_acquire(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_shm_release(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_release(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_mark_frames_done(ctx, args) requires(compositor) {
    let present_time_ms = args.arg0;
    video::surface_mark_frames_done(present_time_ms);
    ctx.ok(0)
});

define_syscall!(syscall_roulette_draw(ctx, args) requires(display_exclusive) {
    let fate = args.arg0_u32();
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let disp = ctx.from_result(video::roulette_draw(fate));
    let _ = paging::switch_page_directory(original_dir);
    disp
});

define_syscall!(syscall_roulette_spin(ctx, args) requires(let task_id) {
    let _ = args;
    let res = fate_spin();
    check_result!(ctx, fate_set_pending(res, task_id));
    let packed = ((res.token as u64) << 32) | res.value as u64;
    ctx.ok(packed)
});

define_syscall!(syscall_roulette_result(ctx, args) requires(let task_id) {
    let mut stored = FateResult { token: 0, value: 0 };
    check_result!(ctx, fate_take_pending(task_id, &mut stored));

    let token = (args.arg0 >> 32) as u32;
    if token != stored.token {
        return ctx.err();
    }

    let is_win = (stored.value & 1) == 1;

    if is_win {
        fate_apply_outcome(&stored as *const FateResult, 0, true);
        ctx.ok(0)
    } else {
        fate_apply_outcome(&stored as *const FateResult, 0, false);
        platform::kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const c_char);
    }
});

define_syscall!(syscall_user_write(ctx, args) {
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    require_nonzero!(ctx, args.arg0);
    let write_len = try_or_err!(ctx, syscall_bounded_from_user(&mut tmp, args.arg0, args.arg1, USER_IO_MAX_BYTES));
    platform::console_puts(&tmp[..write_len]);
    ctx.ok(write_len as u64)
});

define_syscall!(syscall_user_read(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = args.arg1_usize().min(USER_IO_MAX_BYTES);

    let mut read_len = tty::read_line(tmp.as_mut_ptr(), max_len);
    if max_len > 0 {
        read_len = read_len.min(max_len.saturating_sub(1));
        tmp[read_len] = 0;
    }

    let copy_len = read_len.saturating_add(1).min(max_len);
    try_or_err!(ctx, syscall_copy_to_user_bounded(args.arg0, &tmp[..copy_len]));
    ctx.ok(read_len as u64)
});

define_syscall!(syscall_user_read_char(ctx, args) {
    let _ = args;
    let mut c = 0u8;
    check_result!(ctx, tty::read_char_blocking(&mut c as *mut u8));
    ctx.ok(c as u64)
});

define_syscall!(syscall_fb_info(ctx, args) {
    let display_info = some_or_err!(ctx, video::get_display_info());
    let user_ptr = try_or_err!(ctx, UserPtr::<DisplayInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &display_info));
    ctx.ok(0)
});

define_syscall!(syscall_sys_info(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    let mut info = UserSysInfo {
        total_pages: 0,
        free_pages: 0,
        allocated_pages: 0,
        total_tasks: 0,
        active_tasks: 0,
        task_context_switches: 0,
        scheduler_context_switches: 0,
        scheduler_yields: 0,
        ready_tasks: 0,
        schedule_calls: 0,
    };

    get_page_allocator_stats(
        &mut info.total_pages,
        &mut info.free_pages,
        &mut info.allocated_pages,
    );
    get_task_stats(
        &mut info.total_tasks,
        &mut info.active_tasks,
        &mut info.task_context_switches,
    );
    get_scheduler_stats(
        &mut info.scheduler_context_switches,
        &mut info.scheduler_yields,
        &mut info.ready_tasks,
        &mut info.schedule_calls,
    );

    let user_ptr = try_or_err!(ctx, UserPtr::<UserSysInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &info));
    ctx.ok(0)
});

define_syscall!(syscall_spawn_path(ctx, args) {
    let path_ptr = args.arg0 as *const u8;
    let path_len = args.arg1 as usize;
    let priority = args.arg2 as u8;
    let flags = args.arg3 as u16;

    if path_ptr.is_null() || path_len == 0 || path_len > exec::EXEC_MAX_PATH {
        wl_currency::award_loss();
        return ctx.err();
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    let copied_len = match syscall_bounded_from_user(
        &mut path_buf,
        path_ptr as u64,
        path_len as u64,
        exec::EXEC_MAX_PATH,
    ) {
        Ok(len) => len,
        Err(_) => {
            wl_currency::award_loss();
            return ctx.err();
        }
    };

    match exec::spawn_program_with_attrs(&path_buf[..copied_len], priority, flags) {
        Ok(task_id) => ctx.ok(task_id as u64),
        Err(err) => ctx.ok(err as i32 as u64),
    }
});

define_syscall!(syscall_waitpid(ctx, args) {
    let target_id = args.arg0 as u32;
    if target_id == 0 || target_id == INVALID_TASK_ID {
        return ctx.err();
    }

    let mut record = TaskExitRecord::empty();
    if task_get_exit_record(target_id, &mut record) == 0 {
        return ctx.ok(record.exit_code as u64);
    }

    // Block caller until target terminates via release_task_dependents wake path
    task_wait_for(target_id);

    let mut record2 = TaskExitRecord::empty();
    if task_get_exit_record(target_id, &mut record2) == 0 {
        ctx.ok(record2.exit_code as u64)
    } else {
        ctx.ok(0)
    }
});

define_syscall!(syscall_terminate_task(ctx, args) requires(compositor) {
    let target_id = args.arg0_u32();
    if target_id == 0 || target_id == INVALID_TASK_ID {
        wl_currency::award_loss();
        return ctx.err();
    }

    let caller_id = ctx.task_id().unwrap_or(INVALID_TASK_ID);
    if target_id == caller_id {
        wl_currency::award_loss();
        return ctx.err();
    }

    if task_terminate(target_id) != 0 {
        wl_currency::award_loss();
        return ctx.err();
    }

    wl_currency::award_win();
    ctx.ok(0)
});

pub fn syscall_exec(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let process_id = match ctx.require_process_id() {
        Ok(id) => id,
        Err(d) => return d,
    };

    let args = ctx.args();
    let path_ptr = args.arg0;

    if path_ptr == 0 {
        wl_currency::award_loss();
        return ctx.err();
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    if syscall_copy_user_str(&mut path_buf, path_ptr).is_err() {
        wl_currency::award_loss();
        return ctx.err();
    }

    let path_len = path_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path_buf.len());
    let path = &path_buf[..path_len];

    let mut entry_point = 0u64;
    let mut stack_ptr = 0u64;

    match exec::do_exec(
        process_id,
        path,
        None,
        None,
        &mut entry_point,
        &mut stack_ptr,
    ) {
        Ok(()) => {
            wl_currency::award_win();
            unsafe {
                (*frame).rip = entry_point;
                (*frame).rsp = stack_ptr;
                (*frame).rax = 0;
                (*frame).rdi = 0;
                (*frame).rsi = 0;
                (*frame).rdx = 0;
                (*frame).rcx = 0;
                (*frame).r8 = 0;
                (*frame).r9 = 0;
                (*frame).r10 = 0;
                (*frame).r11 = 0;
            }
            SyscallDisposition::Ok
        }
        Err(e) => {
            wl_currency::award_loss();
            unsafe {
                (*frame).rax = e as i32 as u64;
            }
            SyscallDisposition::Ok
        }
    }
}

define_syscall!(syscall_brk(ctx, args) requires(let process_id) {
    let new_brk = args.arg0;
    let result = slopos_mm::process_vm::process_vm_brk(process_id, new_brk);
    ctx.ok(result)
});

define_syscall!(syscall_get_cpu_count(ctx, args) {
    let _ = args;
    ctx.ok(slopos_lib::get_cpu_count() as u64)
});

define_syscall!(syscall_get_current_cpu(ctx, args) {
    let _ = args;
    ctx.ok(slopos_lib::get_current_cpu() as u64)
});

define_syscall!(syscall_set_cpu_affinity(ctx, args) requires(let task_id) {
    let target_or_zero = args.arg0_u32();
    let new_affinity = args.arg1_u32();
    let resolved_task_id = if target_or_zero == 0 { task_id } else { target_or_zero };

    let task_ptr = crate::scheduler::task::task_find_by_id(resolved_task_id);
    if task_ptr.is_null() {
        return ctx.err();
    }

    unsafe { (*task_ptr).cpu_affinity = new_affinity; }
    ctx.ok(0)
});

define_syscall!(syscall_get_cpu_affinity(ctx, args) requires(let task_id) {
    let target_or_zero = args.arg0_u32();
    let resolved_task_id = if target_or_zero == 0 { task_id } else { target_or_zero };

    let task_ptr = crate::scheduler::task::task_find_by_id(resolved_task_id);
    if task_ptr.is_null() {
        return ctx.err();
    }

    ctx.ok(unsafe { (*task_ptr).cpu_affinity } as u64)
});

pub fn syscall_fork(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let child_id = crate::scheduler::task::task_fork(task);
    ctx.from_bool_value(
        child_id != slopos_abi::task::INVALID_TASK_ID,
        child_id as u64,
    )
}

/// Build the static syscall dispatch table from a compact registration list.
///
/// Each entry maps a syscall number constant to its handler function and a
/// debug name string. Unregistered slots remain `{ handler: None, name: null }`.
macro_rules! syscall_table {
    (size: $size:expr; $( [$num:expr] => $handler:expr, $name:literal; )*) => {{
        let mut table: [SyscallEntry; $size] = [SyscallEntry {
            handler: None,
            name: core::ptr::null(),
        }; $size];
        $(
            table[$num as usize] = SyscallEntry {
                handler: Some($handler),
                name: concat!($name, "\0").as_ptr() as *const c_char,
            };
        )*
        table
    }};
}

static SYSCALL_TABLE: [SyscallEntry; 128] = syscall_table! {
    size: 128;

    // Core
    [SYSCALL_YIELD]          => syscall_yield,          "yield";
    [SYSCALL_EXIT]           => syscall_exit,           "exit";
    [SYSCALL_WRITE]          => syscall_user_write,     "write";
    [SYSCALL_READ]           => syscall_user_read,      "read";
    [SYSCALL_READ_CHAR]      => syscall_user_read_char, "read_char";
    [SYSCALL_SLEEP_MS]       => syscall_sleep_ms,       "sleep_ms";
    [SYSCALL_FB_INFO]        => syscall_fb_info,        "fb_info";
    [SYSCALL_GET_TIME_MS]    => syscall_get_time_ms,    "get_time_ms";
    [SYSCALL_SYS_INFO]       => syscall_sys_info,       "sys_info";
    [SYSCALL_HALT]           => syscall_halt,            "halt";
    [SYSCALL_REBOOT]         => syscall_reboot,          "reboot";

    // Random / Roulette
    [SYSCALL_RANDOM_NEXT]     => syscall_random_next,     "random_next";
    [SYSCALL_ROULETTE]        => syscall_roulette_spin,   "roulette";
    [SYSCALL_ROULETTE_RESULT] => syscall_roulette_result, "roulette_result";
    [SYSCALL_ROULETTE_DRAW]   => syscall_roulette_draw,   "roulette_draw";

    // Filesystem
    [SYSCALL_FS_OPEN]   => syscall_fs_open,   "fs_open";
    [SYSCALL_FS_CLOSE]  => syscall_fs_close,  "fs_close";
    [SYSCALL_FS_READ]   => syscall_fs_read,   "fs_read";
    [SYSCALL_FS_WRITE]  => syscall_fs_write,  "fs_write";
    [SYSCALL_FS_STAT]   => syscall_fs_stat,   "fs_stat";
    [SYSCALL_FS_MKDIR]  => syscall_fs_mkdir,  "fs_mkdir";
    [SYSCALL_FS_UNLINK] => syscall_fs_unlink, "fs_unlink";
    [SYSCALL_FS_LIST]   => syscall_fs_list,   "fs_list";

    // TTY
    [SYSCALL_TTY_SET_FOCUS] => syscall_tty_set_focus, "tty_set_focus";

    // Window management
    [SYSCALL_ENUMERATE_WINDOWS]   => syscall_enumerate_windows,   "enumerate_windows";
    [SYSCALL_SET_WINDOW_POSITION] => syscall_set_window_position, "set_window_position";
    [SYSCALL_SET_WINDOW_STATE]    => syscall_set_window_state,    "set_window_state";
    [SYSCALL_RAISE_WINDOW]        => syscall_raise_window,        "raise_window";

    // Surface / Compositor
    [SYSCALL_SURFACE_COMMIT]      => syscall_surface_commit,      "surface_commit";
    [SYSCALL_SURFACE_ATTACH]      => syscall_surface_attach,      "surface_attach";
    [SYSCALL_SURFACE_FRAME]       => syscall_surface_frame,       "surface_frame";
    [SYSCALL_POLL_FRAME_DONE]     => syscall_poll_frame_done,     "poll_frame_done";
    [SYSCALL_MARK_FRAMES_DONE]    => syscall_mark_frames_done,    "mark_frames_done";
    [SYSCALL_SURFACE_DAMAGE]      => syscall_surface_damage,      "surface_damage";
    [SYSCALL_BUFFER_AGE]          => syscall_buffer_age,          "buffer_age";
    [SYSCALL_SURFACE_SET_ROLE]    => syscall_surface_set_role,    "surface_set_role";
    [SYSCALL_SURFACE_SET_PARENT]  => syscall_surface_set_parent,  "surface_set_parent";
    [SYSCALL_SURFACE_SET_REL_POS] => syscall_surface_set_rel_pos, "surface_set_rel_pos";
    [SYSCALL_SURFACE_SET_TITLE]   => syscall_surface_set_title,   "surface_set_title";
    [SYSCALL_FB_FLIP]             => syscall_fb_flip,             "fb_flip";
    [SYSCALL_DRAIN_QUEUE]         => syscall_drain_queue,         "drain_queue";

    // Shared memory
    [SYSCALL_SHM_CREATE]             => syscall_shm_create,             "shm_create";
    [SYSCALL_SHM_MAP]                => syscall_shm_map,                "shm_map";
    [SYSCALL_SHM_UNMAP]              => syscall_shm_unmap,              "shm_unmap";
    [SYSCALL_SHM_DESTROY]            => syscall_shm_destroy,            "shm_destroy";
    [SYSCALL_SHM_ACQUIRE]            => syscall_shm_acquire,            "shm_acquire";
    [SYSCALL_SHM_RELEASE]            => syscall_shm_release,            "shm_release";
    [SYSCALL_SHM_POLL_RELEASED]      => syscall_shm_poll_released,      "shm_poll_released";
    [SYSCALL_SHM_GET_FORMATS]        => syscall_shm_get_formats,        "shm_get_formats";
    [SYSCALL_SHM_CREATE_WITH_FORMAT] => syscall_shm_create_with_format, "shm_create_with_format";

    // Input
    [SYSCALL_INPUT_POLL]                 => syscall_input_poll,                 "input_poll";
    [SYSCALL_INPUT_POLL_BATCH]           => syscall_input_poll_batch,           "input_poll_batch";
    [SYSCALL_INPUT_HAS_EVENTS]           => syscall_input_has_events,           "input_has_events";
    [SYSCALL_INPUT_SET_FOCUS]            => syscall_input_set_focus,            "input_set_focus";
    [SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET] => syscall_input_set_focus_with_offset, "input_set_focus_with_offset";
    [SYSCALL_INPUT_GET_POINTER_POS]      => syscall_input_get_pointer_pos,      "input_get_pointer_pos";
    [SYSCALL_INPUT_GET_BUTTON_STATE]     => syscall_input_get_button_state,     "input_get_button_state";
    [SYSCALL_INPUT_REQUEST_CLOSE]        => syscall_input_request_close,        "input_request_close";

    // Task management
    [SYSCALL_SPAWN_PATH]     => syscall_spawn_path,     "spawn_path";
    [SYSCALL_WAITPID]        => syscall_waitpid,        "waitpid";
    [SYSCALL_TERMINATE_TASK] => syscall_terminate_task,  "terminate_task";
    [SYSCALL_EXEC]           => syscall_exec,            "exec";
    [SYSCALL_FORK]           => syscall_fork,            "fork";

    // Memory
    [SYSCALL_BRK] => syscall_brk, "brk";

    // SMP / CPU affinity
    [SYSCALL_GET_CPU_COUNT]    => syscall_get_cpu_count,    "get_cpu_count";
    [SYSCALL_GET_CURRENT_CPU]  => syscall_get_current_cpu,  "get_current_cpu";
    [SYSCALL_SET_CPU_AFFINITY] => syscall_set_cpu_affinity, "set_cpu_affinity";
    [SYSCALL_GET_CPU_AFFINITY] => syscall_get_cpu_affinity, "get_cpu_affinity";
};

pub fn syscall_lookup(sysno: u64) -> *const SyscallEntry {
    if (sysno as usize) >= SYSCALL_TABLE.len() {
        return ptr::null();
    }
    let entry = &SYSCALL_TABLE[sysno as usize];
    if entry.handler.is_none() {
        ptr::null()
    } else {
        entry as *const SyscallEntry
    }
}
