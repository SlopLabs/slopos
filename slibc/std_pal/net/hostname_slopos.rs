//! `std::net::hostname` for SlopOS: the `nodename` field of `uname(2)`.

use crate::ffi::OsString;
use crate::io::{Error, Result};

/// `slopos_abi::syscall::UserUtsname` — six NUL-terminated 65-byte fields.
const UTS_FIELD_LEN: usize = 65;
const UTS_FIELDS: usize = 6;
const UTS_NODENAME: usize = 1;

unsafe extern "C" {
    fn slopos_uname(out: *mut u8) -> i32;
}

pub fn hostname() -> Result<OsString> {
    let mut uts = [0u8; UTS_FIELD_LEN * UTS_FIELDS];
    let rc = unsafe { slopos_uname(uts.as_mut_ptr()) };
    if rc < 0 {
        return Err(Error::from_raw_os_error(-rc));
    }

    let field = &uts[UTS_NODENAME * UTS_FIELD_LEN..(UTS_NODENAME + 1) * UTS_FIELD_LEN];
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    Ok(OsString::from(
        String::from_utf8_lossy(&field[..len]).into_owned(),
    ))
}
