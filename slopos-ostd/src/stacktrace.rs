#[repr(C)]
#[derive(Copy, Clone)]
pub struct StacktraceEntry {
    pub frame_pointer: u64,
    pub return_address: u64,
}

const KERNEL_ADDR_MIN: u64 = 0xffff_8000_0000_0000;

fn is_canonical_address(address: u64) -> bool {
    let upper = address >> 47;
    upper == 0 || upper == 0x1FFFF
}

#[inline]
fn is_kernel_address(address: u64) -> bool {
    is_canonical_address(address) && address >= KERNEL_ADDR_MIN
}

fn basic_sanity_check(current_rbp: u64, next_rbp: u64) -> bool {
    if next_rbp <= current_rbp {
        return false;
    }
    if next_rbp - current_rbp > (1u64 << 20) {
        return false;
    }
    true
}

/// Walk the frame-pointer chain from `rbp` into `entries`, answering how many
/// frames it filled.
pub fn stacktrace_capture_from(mut rbp: u64, entries: &mut [StacktraceEntry]) -> usize {
    let mut count = 0;

    while rbp != 0 && count < entries.len() {
        if rbp & 0x7 != 0 || !is_kernel_address(rbp) {
            break;
        }
        let Some(ret_slot) = rbp.checked_add(core::mem::size_of::<u64>() as u64) else {
            break;
        };
        if !is_kernel_address(ret_slot) {
            break;
        }

        // A canonical frame pointer can still reference an unmapped page, so
        // the fault-recoverable probe truncates the walk instead of faulting.
        let Some(next_rbp) =
            crate::arch::x86_64::kernel_ptr::read_volatile_canonical_kernel_u64(rbp)
        else {
            break;
        };
        let Some(return_address) =
            crate::arch::x86_64::kernel_ptr::read_volatile_canonical_kernel_u64(ret_slot)
        else {
            break;
        };

        entries[count] = StacktraceEntry {
            frame_pointer: rbp,
            return_address,
        };
        count += 1;

        if next_rbp == 0 {
            break;
        }
        if !is_kernel_address(next_rbp) {
            break;
        }
        if !basic_sanity_check(rbp, next_rbp) {
            break;
        }

        rbp = next_rbp;
    }

    count
}
