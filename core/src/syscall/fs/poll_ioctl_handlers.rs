#![allow(clippy::too_many_arguments)]

use core::ffi::c_int;

use slopos_abi::syscall::{
    POLLIN, POLLOUT, TCGETS, TCSETS, TCSETSF, TCSETSW, TIOCGPGRP, TIOCGWINSZ, TIOCSPGRP,
    UserPollFd, UserTermios, UserTimeval, UserWinsize,
};

use slopos_fs::fileio::{file_is_console_fd, file_poll_fd};

use crate::syscall_services::tty;
use slopos_lib::IrqMutex;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

const SELECT_MAX_FDS: usize = 256;

#[derive(Clone, Copy)]
struct TtyIoctlState {
    termios: UserTermios,
    winsize: UserWinsize,
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
                UserPtr::<UserPollFd>::try_new(
                    base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64
                )
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
        tv.tv_sec
            .saturating_mul(1000)
            .saturating_add(tv.tv_usec / 1000)
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
            let fg_pgrp = tty::get_foreground_pgrp();
            try_or_err!(ctx, copy_to_user(ptr, &fg_pgrp));
            ctx.ok(0)
        }
        TIOCSPGRP => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let pgrp = try_or_err!(ctx, copy_from_user(ptr));
            ctx.from_bool_value(tty::set_foreground_pgrp(pgrp) == 0, 0)
        }
        _ => ctx.err(),
    }
});
