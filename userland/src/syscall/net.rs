use super::numbers::SYSCALL_NET_SCAN;
use super::raw::syscall3;
use slopos_abi::net::UserNetMember;

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
