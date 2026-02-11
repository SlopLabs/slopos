#![allow(clippy::too_many_arguments)]

use core::ffi::{c_char, c_int, c_void};
use core::mem;

use slopos_abi::syscall::{
    O_CLOEXEC, O_NONBLOCK, POLLIN, POLLOUT, TCGETS, TCSETS, TCSETSF, TCSETSW, TIOCGPGRP,
    TIOCGWINSZ, TIOCSPGRP, UserPollFd, UserTermios, UserTimeval, UserWinsize,
};
use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};

use crate::syscall::common::{
    USER_IO_MAX_BYTES, USER_PATH_MAX, syscall_bounded_from_user, syscall_copy_to_user_bounded,
    syscall_copy_user_str_to_cstr,
};

use slopos_fs::fileio::{
    file_close_fd, file_dup_fd, file_dup2_fd, file_dup3_fd, file_fcntl_fd, file_fstat_fd,
    file_is_console_fd, file_list_path, file_mkdir_path, file_open_for_process, file_pipe_create,
    file_poll_fd, file_read_fd, file_seek_fd, file_stat_path, file_unlink_path, file_write_fd,
};

use slopos_lib::IrqMutex;
use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

const SELECT_MAX_FDS: usize = 256;

#[derive(Clone, Copy)]
struct TtyIoctlState {
    termios: UserTermios,
    winsize: UserWinsize,
    fg_pgrp: u32,
}

impl TtyIoctlState {
    const fn new() -> Self {
        Self {
            termios: UserTermios {
                c_iflag: 0,
                c_oflag: 0,
                c_cflag: 0,
                c_lflag: 0,
                c_line: 0,
                c_cc: [0; slopos_abi::syscall::NCCS],
                c_ispeed: 0,
                c_ospeed: 0,
            },
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            fg_pgrp: 0,
        }
    }
}

static TTY_IOCTL_STATE: IrqMutex<TtyIoctlState> = IrqMutex::new(TtyIoctlState::new());

#[inline]
fn fdset_bytes_len(nfds: usize) -> usize {
    nfds.div_ceil(8)
}

fn fdset_test(buf: &[u8], fd: usize) -> bool {
    let byte = fd / 8;
    let bit = fd % 8;
    if byte >= buf.len() {
        return false;
    }
    (buf[byte] & (1u8 << bit)) != 0
}

fn fdset_set(buf: &mut [u8], fd: usize) {
    let byte = fd / 8;
    let bit = fd % 8;
    if byte < buf.len() {
        buf[byte] |= 1u8 << bit;
    }
}

fn poll_to_select_mask(
    revents: u16,
    read_set: bool,
    write_set: bool,
    except_set: bool,
) -> (bool, bool, bool) {
    let read_ready = read_set
        && (revents & (POLLIN | slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0;
    let write_ready = write_set && (revents & (POLLOUT | slopos_abi::syscall::POLLERR)) != 0;
    let except_ready = except_set && (revents & slopos_abi::syscall::POLLPRI) != 0;
    (read_ready, write_ready, except_ready)
}

define_syscall!(syscall_fs_open(ctx, args) requires(let pid: process_id) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    let fd = file_open_for_process(pid, path.as_ptr(), args.arg1_u32());
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_fs_close(ctx, args) requires(let pid: process_id) {
    ctx.from_zero_success(file_close_fd(pid, args.arg0 as c_int))
});

define_syscall!(syscall_fs_read(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let capped_len = args.arg2_usize().min(USER_IO_MAX_BYTES);

    let bytes = file_read_fd(pid, args.arg0 as c_int, tmp.as_mut_ptr() as *mut c_char, capped_len);
    if bytes < 0 {
        return ctx.err();
    }

    try_or_err!(ctx, syscall_copy_to_user_bounded(args.arg1, &tmp[..bytes as usize]));
    ctx.ok(bytes as u64)
});

define_syscall!(syscall_fs_write(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let write_len = try_or_err!(ctx, syscall_bounded_from_user(&mut tmp, args.arg1, args.arg2, USER_IO_MAX_BYTES));

    let bytes = file_write_fd(pid, args.arg0 as c_int, tmp.as_ptr() as *const c_char, write_len);
    ctx.from_rc_value(bytes as i64)
});

define_syscall!(syscall_fs_stat(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));

    let mut stat = UserFsStat { type_: 0, size: 0 };
    check_result!(ctx, file_stat_path(path.as_ptr(), &mut stat.type_, &mut stat.size));

    let stat_ptr = try_or_err!(ctx, UserPtr::<UserFsStat>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(stat_ptr, &stat));
    ctx.ok(0)
});

define_syscall!(syscall_fs_mkdir(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_mkdir_path(path.as_ptr()))
});

define_syscall!(syscall_fs_unlink(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_unlink_path(path.as_ptr()))
});

define_syscall!(syscall_fs_list(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    require_nonzero!(ctx, args.arg1);

    let list_hdr_ptr = try_or_err!(ctx, UserPtr::<UserFsList>::try_new(args.arg1));
    let mut list_hdr = try_or_err!(ctx, copy_from_user(list_hdr_ptr));

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return ctx.err();
    }

    let tmp_size = mem::size_of::<UserFsEntry>() * cap as usize;
    let tmp_ptr = kmalloc(tmp_size) as *mut UserFsEntry;
    require_nonnull!(ctx, tmp_ptr);
    unsafe { core::ptr::write_bytes(tmp_ptr as *mut u8, 0, tmp_size); }

    let mut count: u32 = 0;
    let rc = file_list_path(path.as_ptr(), tmp_ptr, cap, &mut count);
    if rc != 0 {
        kfree(tmp_ptr as *mut c_void);
        return ctx.err();
    }

    list_hdr.count = count;

    let entries_bytes = unsafe {
        core::slice::from_raw_parts(tmp_ptr as *const u8, mem::size_of::<UserFsEntry>() * count as usize)
    };
    let entries_user = match UserBytes::try_new(list_hdr.entries as u64, entries_bytes.len()) {
        Ok(b) => b,
        Err(_) => {
            kfree(tmp_ptr as *mut c_void);
            return ctx.err();
        }
    };

    let rc_entries = copy_bytes_to_user(entries_user, entries_bytes);
    let rc_hdr = if rc_entries.is_ok() {
        let hdr_ptr = match UserPtr::<UserFsList>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => {
                kfree(tmp_ptr as *mut c_void);
                return ctx.err();
            }
        };
        copy_to_user(hdr_ptr, &list_hdr)
    } else {
        rc_entries.map(|_| ())
    };

    kfree(tmp_ptr as *mut c_void);
    ctx.from_result(rc_hdr)
});

// =============================================================================
// FD operations: dup, dup2, dup3, fcntl, lseek, fstat
// =============================================================================

define_syscall!(syscall_dup(ctx, args) requires(let pid: process_id) {
    let fd = file_dup_fd(pid, args.arg0 as c_int);
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_dup2(ctx, args) requires(let pid: process_id) {
    let fd = file_dup2_fd(pid, args.arg0 as c_int, args.arg1 as c_int);
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_dup3(ctx, args) requires(let pid: process_id) {
    let fd = file_dup3_fd(pid, args.arg0 as c_int, args.arg1 as c_int, args.arg2_u32());
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_fcntl(ctx, args) requires(let pid: process_id) {
    let rc = file_fcntl_fd(pid, args.arg0 as c_int, args.arg1, args.arg2);
    if rc < 0 {
        ctx.err()
    } else {
        ctx.ok(rc as u64)
    }
});

define_syscall!(syscall_lseek(ctx, args) requires(let pid: process_id) {
    let new_offset = file_seek_fd(pid, args.arg0 as c_int, args.arg1 as i64, args.arg2_u32());
    if new_offset < 0 {
        ctx.err()
    } else {
        ctx.ok(new_offset as u64)
    }
});

define_syscall!(syscall_fstat(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg1);

    let mut stat = UserFsStat { type_: 0, size: 0 };
    check_result!(ctx, file_fstat_fd(pid, args.arg0 as c_int, &mut stat));

    let stat_ptr = try_or_err!(ctx, UserPtr::<UserFsStat>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(stat_ptr, &stat));
    ctx.ok(0)
});

define_syscall!(syscall_pipe(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg0);
    let out_fds = try_or_err!(ctx, UserPtr::<[i32; 2]>::try_new(args.arg0));
    let mut read_fd: c_int = -1;
    let mut write_fd: c_int = -1;
    check_result!(ctx, file_pipe_create(pid, 0, &mut read_fd, &mut write_fd));
    let pair = [read_fd, write_fd];
    try_or_err!(ctx, copy_to_user(out_fds, &pair));
    ctx.ok(0)
});

define_syscall!(syscall_pipe2(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg0);
    let flags = args.arg1 as u32;
    if (flags & !(O_CLOEXEC as u32 | O_NONBLOCK as u32)) != 0 {
        return ctx.err();
    }
    let out_fds = try_or_err!(ctx, UserPtr::<[i32; 2]>::try_new(args.arg0));
    let mut read_fd: c_int = -1;
    let mut write_fd: c_int = -1;
    check_result!(ctx, file_pipe_create(pid, flags, &mut read_fd, &mut write_fd));
    let pair = [read_fd, write_fd];
    try_or_err!(ctx, copy_to_user(out_fds, &pair));
    ctx.ok(0)
});

define_syscall!(syscall_poll(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg1_usize();
    let timeout_ms = args.arg2 as i64;

    if args.arg0 == 0 || nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let base_ptr = args.arg0;
    let start_ms = crate::platform::get_time_ms();

    loop {
        let mut ready_count = 0u64;
        for idx in 0..nfds {
            let user_ptr = try_or_err!(
                ctx,
                UserPtr::<UserPollFd>::try_new(base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64)
            );
            let mut pfd = try_or_err!(ctx, copy_from_user(user_ptr));
            if pfd.fd < 0 {
                pfd.revents = 0;
            } else {
                pfd.revents = file_poll_fd(pid, pfd.fd as c_int, pfd.events);
                if pfd.revents != 0 {
                    ready_count += 1;
                }
            }
            try_or_err!(ctx, copy_to_user(user_ptr, &pfd));
        }

        if ready_count > 0 {
            return ctx.ok(ready_count);
        }

        if timeout_ms == 0 {
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = crate::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                return ctx.ok(0);
            }
        }

        if crate::sched::scheduler_is_preemption_enabled() != 0 {
            crate::sched::sleep_current_task_ms(1);
        } else {
            crate::platform::timer_poll_delay_ms(1);
        }
    }
});

define_syscall!(syscall_select(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg0_usize();
    if nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let bytes_len = fdset_bytes_len(nfds);
    let mut read_in = [0u8; SELECT_MAX_FDS / 8];
    let mut write_in = [0u8; SELECT_MAX_FDS / 8];
    let mut except_in = [0u8; SELECT_MAX_FDS / 8];
    let mut read_out = [0u8; SELECT_MAX_FDS / 8];
    let mut write_out = [0u8; SELECT_MAX_FDS / 8];
    let mut except_out = [0u8; SELECT_MAX_FDS / 8];

    if args.arg1 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut read_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg2 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut write_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg3 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut except_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }

    let timeout_ms = if args.arg4 == 0 {
        -1i64
    } else {
        let tv_ptr = try_or_err!(ctx, UserPtr::<UserTimeval>::try_new(args.arg4));
        let tv = try_or_err!(ctx, copy_from_user(tv_ptr));
        if tv.tv_sec < 0 || tv.tv_usec < 0 {
            return ctx.err();
        }
        tv.tv_sec.saturating_mul(1000).saturating_add(tv.tv_usec / 1000)
    };

    let start_ms = crate::platform::get_time_ms();
    loop {
        read_out[..bytes_len].fill(0);
        write_out[..bytes_len].fill(0);
        except_out[..bytes_len].fill(0);
        let mut ready = 0u64;

        for fd in 0..nfds {
            let want_r = args.arg1 != 0 && fdset_test(&read_in[..bytes_len], fd);
            let want_w = args.arg2 != 0 && fdset_test(&write_in[..bytes_len], fd);
            let want_e = args.arg3 != 0 && fdset_test(&except_in[..bytes_len], fd);
            if !(want_r || want_w || want_e) {
                continue;
            }

            let mut mask = 0u16;
            if want_r {
                mask |= POLLIN;
            }
            if want_w {
                mask |= POLLOUT;
            }
            if want_e {
                mask |= slopos_abi::syscall::POLLPRI;
            }

            let revents = file_poll_fd(pid, fd as c_int, mask);
            let (rdy_r, rdy_w, rdy_e) = poll_to_select_mask(revents, want_r, want_w, want_e);
            if rdy_r {
                fdset_set(&mut read_out[..bytes_len], fd);
                ready += 1;
            }
            if rdy_w {
                fdset_set(&mut write_out[..bytes_len], fd);
                ready += 1;
            }
            if rdy_e {
                fdset_set(&mut except_out[..bytes_len], fd);
                ready += 1;
            }
        }

        if ready > 0 {
            if args.arg1 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
            }
            if args.arg2 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
            }
            if args.arg3 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
            }
            return ctx.ok(ready);
        }

        if timeout_ms == 0 {
            if args.arg1 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
            }
            if args.arg2 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
            }
            if args.arg3 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
            }
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = crate::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                if args.arg1 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
                }
                if args.arg2 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
                }
                if args.arg3 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
                }
                return ctx.ok(0);
            }
        }

        if crate::sched::scheduler_is_preemption_enabled() != 0 {
            crate::sched::sleep_current_task_ms(1);
        } else {
            crate::platform::timer_poll_delay_ms(1);
        }
    }
});

define_syscall!(syscall_ioctl(ctx, args) requires(let pid: process_id) {
    let fd = args.arg0 as c_int;
    let cmd = args.arg1;
    let arg = args.arg2;

    if !file_is_console_fd(pid, fd) {
        return ctx.err();
    }

    match cmd {
        TCGETS => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let state = *TTY_IOCTL_STATE.lock();
            try_or_err!(ctx, copy_to_user(ptr, &state.termios));
            ctx.ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            TTY_IOCTL_STATE.lock().termios = val;
            ctx.ok(0)
        }
        TIOCGWINSZ => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserWinsize>::try_new(arg));
            let state = *TTY_IOCTL_STATE.lock();
            try_or_err!(ctx, copy_to_user(ptr, &state.winsize));
            ctx.ok(0)
        }
        slopos_abi::syscall::TIOCSWINSZ => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserWinsize>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            TTY_IOCTL_STATE.lock().winsize = val;
            ctx.ok(0)
        }
        TIOCGPGRP => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let state = *TTY_IOCTL_STATE.lock();
            try_or_err!(ctx, copy_to_user(ptr, &state.fg_pgrp));
            ctx.ok(0)
        }
        TIOCSPGRP => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let pgrp = try_or_err!(ctx, copy_from_user(ptr));
            TTY_IOCTL_STATE.lock().fg_pgrp = pgrp;
            ctx.ok(0)
        }
        _ => ctx.err(),
    }
});
