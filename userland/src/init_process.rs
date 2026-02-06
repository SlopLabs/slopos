use crate::syscall::{core as sys_core, process, tty};

fn spawn_service(name: &[u8]) -> i32 {
    let tid = process::spawn(name);
    if tid <= 0 {
        let _ = tty::write(b"init: failed to spawn service\n");
    }
    tid
}

pub fn init_user_main(_arg: *mut u8) {
    let roulette_tid = spawn_service(b"roulette");
    if roulette_tid > 0 {
        process::waitpid(roulette_tid as u32);
    }

    let compositor_tid = spawn_service(b"compositor");
    spawn_service(b"shell");

    // Block on compositor — it runs forever so init stays dormant (zero CPU).
    // Like real PID 1: wait for children, don't busy-loop.
    if compositor_tid > 0 {
        process::waitpid(compositor_tid as u32);
    }

    // Compositor died — keep init alive as a fallback reaper.
    loop {
        sys_core::yield_now();
    }
}
