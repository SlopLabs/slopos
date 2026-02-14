use core::ffi::c_int;

use slopos_abi::UserFsStat;
use slopos_abi::syscall::{O_CLOEXEC, O_NONBLOCK};

use slopos_fs::fileio::{
    file_dup_fd, file_dup2_fd, file_dup3_fd, file_fcntl_fd, file_fstat_fd, file_pipe_create,
    file_seek_fd,
};

use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

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
    ctx.from_rc_value(rc)
});

define_syscall!(syscall_lseek(ctx, args) requires(let pid: process_id) {
    let new_offset = file_seek_fd(pid, args.arg0 as c_int, args.arg1 as i64, args.arg2_u32());
    ctx.from_rc_value(new_offset)
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
