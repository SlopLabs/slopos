use slopos_abi::syscall::UserBlockStats;

slopos_service_core::define_service! {
    block => BlockServices {
        disk_count() -> usize;
        disk_stats(out: &mut [UserBlockStats]) -> usize;
    }
}
