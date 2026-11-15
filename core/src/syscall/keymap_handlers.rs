//! Keyboard-layout syscall handlers.
//!
//! Thin forwarders to the keymap service the keyboard driver registers; the
//! active layout lives in the driver, which `core` cannot depend on directly.
//!
//! Loading is console administration: the one layout table feeds every TTY and
//! the compositor's input path, not a single client's view of a seat.

use slopos_abi::Errno;
use slopos_kernel_services::syscall_services::keymap;

define_syscall!(syscall_keymap_load
    (ctx, data_ptr: u64, len: u64)
    cap(ConsoleConfig)
    -> Result<(), Errno>
{
    keymap::load(data_ptr, len as usize)
});
