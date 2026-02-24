use crate::program_registry;
use crate::runtime;
use crate::syscall::{UserSysInfo, core as sys_core, process};

use super::super::display::{
    COLOR_COMMENT_GRAY, COLOR_ERROR_RED, COLOR_EXEC_GREEN, COLOR_PROMPT_ACCENT,
    shell_console_clear, shell_write, shell_write_idx,
};
use super::super::jobs::write_u64;
use super::super::parser::u_streq_slice;
use super::super::{HALTED, NL, REBOOTING};
use super::{BUILTINS, BuiltinCategory, print_kv};

const NAME_COL_WIDTH: usize = 12;
const PADDING: &[u8] = b"            ";

fn write_padded_colored(name: &[u8], color: u8) {
    shell_write_idx(name, color);
    let pad = NAME_COL_WIDTH.saturating_sub(name.len());
    if pad > 0 {
        shell_write(&PADDING[..pad]);
    }
}

pub fn cmd_help(argc: i32, argv: &[*const u8]) -> i32 {
    if argc >= 2 && !argv[1].is_null() {
        return cmd_help_single(argv[1]);
    }

    shell_write_idx(b"SlopOS Shell v0.2\n", COLOR_PROMPT_ACCENT);
    shell_write(b"Type 'help <command>' for detailed usage.\n\n");

    for &cat in BuiltinCategory::ALL {
        shell_write_idx(cat.label(), COLOR_PROMPT_ACCENT);
        shell_write(b":\n");
        for entry in BUILTINS {
            if entry.category != cat {
                continue;
            }
            shell_write(b"  ");
            write_padded_colored(entry.name, COLOR_EXEC_GREEN);
            shell_write(entry.desc);
            shell_write(NL);
        }
        shell_write(NL);
    }

    shell_write_idx(b"Programs", COLOR_PROMPT_ACCENT);
    shell_write(b":\n");
    for spec in program_registry::user_programs() {
        shell_write(b"  ");
        write_padded_colored(spec.name, COLOR_EXEC_GREEN);
        shell_write(spec.desc);
        shell_write(NL);
    }
    shell_write(NL);

    0
}

fn cmd_help_single(name: *const u8) -> i32 {
    for entry in BUILTINS {
        if !u_streq_slice(name, entry.name) {
            continue;
        }
        shell_write_idx(entry.name, COLOR_EXEC_GREEN);
        shell_write(b" - ");
        shell_write(entry.desc);
        shell_write(b"\n\n");
        shell_write_idx(b"Usage: ", COLOR_COMMENT_GRAY);
        shell_write(entry.usage);
        shell_write(b"\n\n");
        if !entry.detail.is_empty() {
            shell_write(entry.detail);
            shell_write(NL);
        }
        return 0;
    }

    for spec in program_registry::user_programs() {
        if !u_streq_slice(name, spec.name) {
            continue;
        }
        shell_write_idx(spec.name, COLOR_EXEC_GREEN);
        shell_write(b" - ");
        shell_write(spec.desc);
        shell_write(NL);
        return 0;
    }

    shell_write_idx(b"help: unknown command '", COLOR_ERROR_RED);
    let len = runtime::u_strlen(name);
    shell_write_idx(
        unsafe { core::slice::from_raw_parts(name, len) },
        COLOR_ERROR_RED,
    );
    shell_write_idx(b"'\n", COLOR_ERROR_RED);
    1
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
        shell_write_idx(b"info: failed\n", COLOR_ERROR_RED);
        return 1;
    }
    shell_write_idx(b"Kernel information:\n", COLOR_PROMPT_ACCENT);
    shell_write_idx(b"  Memory: total pages=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.total_pages as u64);
    shell_write_idx(b"  Free pages=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.free_pages as u64);
    shell_write_idx(b"  Allocated pages=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.allocated_pages as u64);
    shell_write_idx(b"  Tasks: total=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.total_tasks as u64);
    shell_write_idx(b"  Active tasks=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.active_tasks as u64);
    shell_write_idx(b"  Task ctx switches=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.task_context_switches);
    shell_write_idx(b"  Scheduler: switches=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.scheduler_context_switches);
    shell_write_idx(b"  Yields=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.scheduler_yields);
    shell_write_idx(b"  Ready=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.ready_tasks as u64);
    shell_write_idx(b"  schedule() calls=", COLOR_COMMENT_GRAY);
    print_kv(b"", info.schedule_calls as u64);
    0
}

fn write_zero_padded(buf: &mut [u8], pos: usize, value: u64) {
    if pos + 1 < buf.len() {
        buf[pos] = b'0' + ((value / 10) % 10) as u8;
        buf[pos + 1] = b'0' + (value % 10) as u8;
    }
}

pub fn cmd_uptime(_argc: i32, _argv: &[*const u8]) -> i32 {
    let ms = sys_core::get_time_ms();
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    shell_write(b"up ");

    if hours > 0 {
        write_u64(hours);
        shell_write(b"h ");
    }

    let mut time_buf = [0u8; 5];
    write_zero_padded(&mut time_buf, 0, minutes);
    time_buf[2] = b':';
    write_zero_padded(&mut time_buf, 3, seconds);
    shell_write(&time_buf);

    shell_write(b" (");
    write_u64(ms);
    shell_write(b" ms)\n");
    0
}

pub fn cmd_cpuinfo(_argc: i32, _argv: &[*const u8]) -> i32 {
    let cpu_count = sys_core::get_cpu_count();
    let current = sys_core::get_current_cpu();

    shell_write_idx(b"Architecture:  ", COLOR_COMMENT_GRAY);
    shell_write(b"x86_64\n");
    shell_write_idx(b"CPU(s):        ", COLOR_COMMENT_GRAY);
    write_u64(cpu_count as u64);
    shell_write(NL);
    shell_write_idx(b"Current CPU:   ", COLOR_COMMENT_GRAY);
    write_u64(current as u64);
    shell_write(NL);
    0
}

pub fn cmd_free(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"free: failed to query system info\n", COLOR_ERROR_RED);
        return 1;
    }

    const PAGE_SIZE_KB: u64 = 4; // 4 KiB per page

    let total_kb = info.total_pages as u64 * PAGE_SIZE_KB;
    let free_kb = info.free_pages as u64 * PAGE_SIZE_KB;
    let used_kb = info.allocated_pages as u64 * PAGE_SIZE_KB;

    shell_write_idx(
        b"              total       free       used\n",
        COLOR_COMMENT_GRAY,
    );

    shell_write_idx(b"Pages:   ", COLOR_COMMENT_GRAY);
    write_right_aligned(info.total_pages as u64, 10);
    write_right_aligned(info.free_pages as u64, 11);
    write_right_aligned(info.allocated_pages as u64, 11);
    shell_write(NL);

    shell_write_idx(b"KiB:     ", COLOR_COMMENT_GRAY);
    write_right_aligned(total_kb, 10);
    write_right_aligned(free_kb, 11);
    write_right_aligned(used_kb, 11);
    shell_write(NL);

    shell_write_idx(b"MiB:     ", COLOR_COMMENT_GRAY);
    write_right_aligned(total_kb / 1024, 10);
    write_right_aligned(free_kb / 1024, 11);
    write_right_aligned(used_kb / 1024, 11);
    shell_write(NL);

    0
}

fn write_right_aligned(value: u64, width: usize) {
    let mut tmp = [0u8; 20];
    let digit_count = format_u64(value, &mut tmp);
    let pad = width.saturating_sub(digit_count);
    for _ in 0..pad {
        shell_write(b" ");
    }
    shell_write(&tmp[..digit_count]);
}

fn format_u64(value: u64, buf: &mut [u8; 20]) -> usize {
    if value == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut n = value;
    let mut rev = [0u8; 20];
    let mut r = 0usize;
    while n != 0 && r < rev.len() {
        rev[r] = b'0' + (n % 10) as u8;
        n /= 10;
        r += 1;
    }
    let mut idx = 0usize;
    while r > 0 && idx < buf.len() {
        buf[idx] = rev[r - 1];
        idx += 1;
        r -= 1;
    }
    idx
}

pub fn cmd_time(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"time: missing command\n", COLOR_ERROR_RED);
        return 1;
    }

    let start = sys_core::get_time_ms();
    let rc = super::super::exec::execute_tokens(argc - 1, &argv[1..]);
    let end = sys_core::get_time_ms();
    let elapsed = end.saturating_sub(start);

    let secs = elapsed / 1000;
    let millis = elapsed % 1000;

    shell_write(b"\nreal\t");
    write_u64(secs);
    shell_write(b".");
    if millis < 100 {
        shell_write(b"0");
    }
    if millis < 10 {
        shell_write(b"0");
    }
    write_u64(millis);
    shell_write(b"s\n");

    rc
}

pub fn cmd_date(_argc: i32, _argv: &[*const u8]) -> i32 {
    let ms = sys_core::get_time_ms();
    let total_secs = ms / 1000;
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    shell_write(b"Day ");
    write_u64(days);
    shell_write(b" ");

    let mut time_buf = [0u8; 8];
    write_zero_padded(&mut time_buf, 0, hours);
    time_buf[2] = b':';
    write_zero_padded(&mut time_buf, 3, minutes);
    time_buf[5] = b':';
    write_zero_padded(&mut time_buf, 6, seconds);
    shell_write(&time_buf);

    shell_write(b" SLT (Sloptopia Local Time)\n");
    0
}

pub fn cmd_uname(argc: i32, argv: &[*const u8]) -> i32 {
    let mut show_all = argc < 2;
    let mut show_sysname = false;
    let mut show_release = false;
    let mut show_machine = false;

    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() || argv[idx].is_null() {
            continue;
        }
        if u_streq_slice(argv[idx], b"-a") {
            show_all = true;
        } else if u_streq_slice(argv[idx], b"-s") {
            show_sysname = true;
        } else if u_streq_slice(argv[idx], b"-r") {
            show_release = true;
        } else if u_streq_slice(argv[idx], b"-m") {
            show_machine = true;
        }
    }

    if !show_sysname && !show_release && !show_machine {
        show_all = true;
    }

    let mut first = true;

    if show_all || show_sysname {
        shell_write(b"SlopOS");
        first = false;
    }
    if show_all || show_release {
        if !first {
            shell_write(b" ");
        }
        shell_write(b"0.2-slop");
        first = false;
    }
    if show_all || show_machine {
        if !first {
            shell_write(b" ");
        }
        shell_write(b"x86_64");
    }

    shell_write(NL);
    0
}

pub fn cmd_whoami(_argc: i32, _argv: &[*const u8]) -> i32 {
    let uid = process::getuid();
    if uid == 0 {
        shell_write(b"root\n");
    } else {
        shell_write(b"uid=");
        write_u64(uid as u64);
        shell_write(NL);
    }
    0
}
