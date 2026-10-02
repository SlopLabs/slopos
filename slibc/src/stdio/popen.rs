//! `popen(3)` and `pclose(3)`: a command run by `/bin/sh -c` with one end of a
//! pipe as its standard output or input, and the other a stream here. The
//! stream's descriptor is close-on-exec, so no later child inherits it, and
//! the shell's process id rides on the stream for `pclose` to wait on.

use core::ffi::c_int;
use core::ptr;

use crate::errno::{EINTR, EINVAL, errno_set};
use crate::pal::{Pal, Sys};
use crate::process::spawn::{
    posix_spawn, posix_spawn_file_actions_adddup2, posix_spawn_file_actions_destroy,
    posix_spawn_file_actions_init,
};
use crate::types::posix_spawn_file_actions_t;

use super::FILE;
use super::file::{fclose, fdopen};

const SHELL: &[u8] = b"/bin/sh\0";
const O_CLOEXEC: u32 = slopos_abi::syscall::O_CLOEXEC as u32;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn popen(command: *const u8, mode: *const u8) -> *mut FILE {
    let direction = if command.is_null() || mode.is_null() {
        None
    } else {
        match *mode {
            b'r' => Some(true),
            b'w' => Some(false),
            _ => None,
        }
    };
    let reading = match direction {
        Some(reading) if matches!(*mode.add(1), 0 | b'e') => reading,
        _ => {
            errno_set(EINVAL.raw());
            return ptr::null_mut();
        }
    };
    let mut pipe = [0 as c_int; 2];
    if let Err(e) = Sys::pipe2(&mut pipe, O_CLOEXEC) {
        errno_set(e.raw());
        return ptr::null_mut();
    }
    let (ours, theirs, child_fd) = if reading {
        (pipe[0], pipe[1], 1)
    } else {
        (pipe[1], pipe[0], 0)
    };
    let mut actions: posix_spawn_file_actions_t = core::mem::zeroed();
    posix_spawn_file_actions_init(&mut actions);
    posix_spawn_file_actions_adddup2(&mut actions, theirs, child_fd);
    let argv = [b"sh\0".as_ptr(), b"-c\0".as_ptr(), command, ptr::null()];
    let mut child = 0;
    let spawned = posix_spawn(
        &mut child,
        SHELL.as_ptr(),
        &actions,
        ptr::null(),
        argv.as_ptr(),
        ptr::null(),
    );
    posix_spawn_file_actions_destroy(&mut actions);
    let _ = Sys::close(theirs);
    if spawned != 0 {
        let _ = Sys::close(ours);
        errno_set(spawned);
        return ptr::null_mut();
    }
    let stream = fdopen(ours, if reading { b"r\0" } else { b"w\0" }.as_ptr());
    if stream.is_null() {
        let _ = Sys::close(ours);
        let mut status = 0;
        while matches!(Sys::waitpid(child, &mut status, 0), Err(e) if e == EINTR) {}
        return ptr::null_mut();
    }
    (*stream).child = child;
    stream
}

/// Close a stream `popen` opened and answer the shell's wait status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pclose(stream: *mut FILE) -> c_int {
    if stream.is_null() || (*stream).child <= 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    let child = (*stream).child;
    fclose(stream);
    let mut status = 0;
    loop {
        match Sys::waitpid(child, &mut status, 0) {
            Ok(_) => return status,
            Err(e) if e == EINTR => {}
            Err(e) => {
                errno_set(e.raw());
                return -1;
            }
        }
    }
}
