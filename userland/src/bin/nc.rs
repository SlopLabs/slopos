#![no_std]
#![no_main]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = slopos_userland::syscall::tty::write(b"panic!\n");
    slopos_userland::syscall::core::exit_with_code(101);
}

/// Entry point for nc — extracts argc/argv from the user stack
/// (placed there by the kernel's exec handler) and dispatches to nc_main.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "mov rdi, [rsp]",       // argc
        "lea rsi, [rsp + 8]",   // argv
        "and rsp, -16",         // 16-byte stack alignment for call
        "call {entry}",
        "ud2",
        entry = sym nc_entry,
    );
}

#[allow(unreachable_code)]
extern "C" fn nc_entry(argc: usize, argv: *const *const u8) -> ! {
    slopos_userland::apps::nc::nc_main_args(argc, argv);
    slopos_userland::syscall::core::exit();
}
