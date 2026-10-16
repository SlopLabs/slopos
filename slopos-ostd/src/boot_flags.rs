use core::sync::atomic::{AtomicU32, Ordering};
pub use slopos_abi::syscall::{
    BOOT_FLAG_EXTENSIONS_OFF, BOOT_FLAG_FATE_REBOOT, BOOT_FLAG_MEDIUM, BOOT_FLAG_PANIC_ON_OOPS,
    BOOT_FLAG_PANIC_RECOVER_SMOKE, BOOT_FLAG_ROOT_PERSISTENT, BOOT_FLAG_ROULETTE_SKIP,
    BOOT_FLAG_TESTS_ENABLED,
};

static FLAGS: AtomicU32 = AtomicU32::new(0);

pub fn set_flag(flag: u32) {
    FLAGS.fetch_or(flag, Ordering::Relaxed);
}

pub fn get_flags() -> u32 {
    FLAGS.load(Ordering::Relaxed)
}

pub fn has_flag(flag: u32) -> bool {
    get_flags() & flag != 0
}
