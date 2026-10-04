//! `/bin/kconsole` — the diagnostic console's informational commands, run
//! from a shell or the remote control rather than the physical keyboard, with
//! what each wrote to the kernel log copied to standard output.
//!
//! The kernel grants this program `TASK_FLAG_PROC_ADMIN` by path. A command
//! that may change the machine stays the physical console's alone; the kernel
//! refuses it here.

use std::io::Write;

use slopos_abi::errno::Errno;

use crate::kmsg;
use crate::syscall::core as sys_core;

const USAGE: &str = "usage: kconsole [KEYS]   one command per key; no keys runs 'h', the list";

pub fn kconsole_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keys: Vec<u8> = match args.as_slice() {
        [] => vec![b'h'],
        [keys] if !keys.is_empty() && keys.is_ascii() => keys.bytes().collect(),
        _ => {
            eprintln!("{USAGE}");
            sys_core::exit_with_code(2)
        }
    };
    let mut status = 0;
    for key in keys {
        if let Err(why) = run(key) {
            eprintln!("kconsole: '{}': {why}", key as char);
            status = 1;
        }
    }
    let _ = std::io::stdout().flush();
    sys_core::exit_with_code(status)
}

fn run(key: u8) -> Result<(), String> {
    let before = kmsg::read().map_err(|e| format!("{}: {e}", kmsg::PATH))?;
    let rc = sys_core::kconsole(key);
    if rc < 0 {
        return Err(match Errno::from_raw(rc as i32) {
            Some(Errno::EPERM) => "refused: the command may change the machine, \
                                   or this program was not started with its grant"
                .into(),
            Some(Errno::ENOENT) => "no command takes this key".into(),
            Some(errno) => errno.description().into(),
            None => format!("error {rc}"),
        });
    }
    let after = kmsg::read().map_err(|e| format!("{}: {e}", kmsg::PATH))?;
    let new = kmsg::added(&before, &after).unwrap_or_else(|| {
        eprintln!("kconsole: the kernel log wrapped past the command's start");
        &after
    });
    std::io::stdout()
        .write_all(new)
        .map_err(|e| format!("standard output: {e}"))
}
