//! System builtin commands: help, echo, clear, info, sysinfo, shutdown, reboot.

use crate::program_registry;
use crate::runtime;
use crate::syscall::{UserSysInfo, core as sys_core, process};

use super::super::display::{shell_console_clear, shell_write};
use super::super::{HALTED, HELP_HEADER, NL, REBOOTING};
use super::{BUILTINS, print_kv};

pub fn cmd_help(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(HELP_HEADER);
    for entry in BUILTINS {
        shell_write(b"  ");
        shell_write(entry.name);
        shell_write(b" - ");
        if !entry.desc.is_empty() {
            shell_write(entry.desc);
        }
        shell_write(NL);
    }
    0
}

pub fn cmd_echo(argc: i32, argv: &[*const u8]) -> i32 {
    let mut first = true;
    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() {
            break;
        }
        let arg = argv[idx];
        if arg.is_null() {
            continue;
        }
        if !first {
            shell_write(b" ");
        }
        let len = runtime::u_strlen(arg);
        shell_write(unsafe { core::slice::from_raw_parts(arg, len) });
        first = false;
    }
    shell_write(NL);
    0
}

pub fn cmd_clear(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(b"\x1B[2J\x1B[H");
    shell_console_clear();
    0
}

pub fn cmd_shutdown(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(HALTED);
    process::halt();
}

pub fn cmd_reboot(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(REBOOTING);
    process::reboot();
}

pub fn cmd_info(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write(b"info: failed\n");
        return 1;
    }
    shell_write(b"Kernel information:\n");
    shell_write(b"  Memory: total pages=");
    print_kv(b"", info.total_pages as u64);
    shell_write(b"  Free pages=");
    print_kv(b"", info.free_pages as u64);
    shell_write(b"  Allocated pages=");
    print_kv(b"", info.allocated_pages as u64);
    shell_write(b"  Tasks: total=");
    print_kv(b"", info.total_tasks as u64);
    shell_write(b"  Active tasks=");
    print_kv(b"", info.active_tasks as u64);
    shell_write(b"  Task ctx switches=");
    print_kv(b"", info.task_context_switches);
    shell_write(b"  Scheduler: switches=");
    print_kv(b"", info.scheduler_context_switches);
    shell_write(b"  Yields=");
    print_kv(b"", info.scheduler_yields);
    shell_write(b"  Ready=");
    print_kv(b"", info.ready_tasks as u64);
    shell_write(b"  schedule() calls=");
    print_kv(b"", info.schedule_calls as u64);
    0
}

pub fn cmd_sysinfo(_argc: i32, _argv: &[*const u8]) -> i32 {
    let rc = match program_registry::resolve_program(b"sysinfo") {
        Some(spec) => process::spawn_path_with_attrs(spec.path, spec.priority, spec.flags),
        None => -1,
    };
    if rc <= 0 {
        shell_write(b"sysinfo: failed to spawn\n");
        return 1;
    }
    0
}
