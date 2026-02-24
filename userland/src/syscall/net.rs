use super::numbers::{SYSCALL_NET_INFO, SYSCALL_NET_SCAN};
use super::raw::{syscall1, syscall3};
use slopos_abi::net::{UserNetInfo, UserNetMember};

#[inline(always)]
pub fn net_scan(out: &mut [UserNetMember], active_probe: bool) -> i64 {
    if out.is_empty() {
        return 0;
    }

    unsafe {
        syscall3(
            SYSCALL_NET_SCAN,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            if active_probe { 1 } else { 0 },
        ) as i64
    }
}

#[inline(always)]
pub fn net_info(out: &mut UserNetInfo) -> i64 {
    unsafe { syscall1(SYSCALL_NET_INFO, out as *mut UserNetInfo as u64) as i64 }
}
