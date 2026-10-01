use super::ondisk::InodeTime;

/// The wall clock as an inode timestamp, or `None` when the bootloader
/// reported no date.
pub fn now() -> Option<InodeTime> {
    slopos_kernel_services::clock::realtime_timespec().map(|(s, ns)| InodeTime::new(s, ns))
}

/// [`now`], or zero, which ext2 reads as unset rather than as 1970.
pub fn now_or_unset() -> InodeTime {
    now().unwrap_or_default()
}

/// Seconds since the Unix epoch for `i_dtime`, which carries no extension,
/// or `0` without a clock.
pub fn now_unix() -> u32 {
    now_unix_opt().unwrap_or(0)
}

/// The wall clock in whole seconds, or `None` when the boot established none.
pub fn now_unix_opt() -> Option<u32> {
    slopos_kernel_services::clock::realtime_unix_secs()
}

/// Stamp a timestamp field only when the clock can answer, so a clockless boot
/// preserves whatever an earlier boot wrote instead of resetting it to zero.
pub fn stamp(field: &mut InodeTime) {
    if let Some(now) = now() {
        *field = now;
    }
}
