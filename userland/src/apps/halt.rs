//! `/bin/halt` — powers the machine off or restarts it, the mechanism of
//! `org.slopos.power.poweroff` and `org.slopos.power.reboot`.
//!
//! Power is not a shell builtin here, for the same reason it is not one in
//! Linux, Redox or Asterinas: the shell asks something that holds the
//! authority. Started without `Power`, this asks privd to run it.
//!
//! `argv[0]` or else the first argument names the action, which is how
//! `halt`, `reboot` and `poweroff` have always been one executable.

use crate::privd::{self, Input};
use crate::syscall::core as sys_core;
use crate::syscall::process;

fn basename(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

pub fn halt_user_main() {
    let mut args = std::env::args();
    let invoked = args
        .next()
        .map(|a| basename(&a).to_string())
        .unwrap_or_else(|| "halt".to_string());
    let verb = args.next().unwrap_or(invoked);

    let action = match verb.as_str() {
        "reboot" => "org.slopos.power.reboot",
        "halt" | "poweroff" | "shutdown" => "org.slopos.power.poweroff",
        other => {
            eprintln!("halt: unknown action '{other}' (want halt, poweroff or reboot)");
            sys_core::exit_with_code(1);
        }
    };
    privd::elevate(Some(action), &[verb.clone().into_bytes()], Input::Closed);

    if verb == "reboot" {
        println!("Rebooting...");
        process::reboot();
    }
    println!("Halting...");
    process::halt();
}
