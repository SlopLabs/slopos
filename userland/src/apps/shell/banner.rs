use crate::syscall::{UserSysInfo, core as sys_core};

use super::display::{
    COLOR_COMMENT_GRAY, COLOR_ERROR_RED, COLOR_EXEC_GREEN, COLOR_PROMPT_ACCENT, COLOR_WARN_YELLOW,
    shell_write, shell_write_idx,
};

const LOGO: &[&[u8]] = &[
    b"   _____ __            ____  _____",
    b"  / ___// /___  ____  / __ \\/ ___/",
    b"  \\__ \\/ / __ \\/ __ \\/ / / /\\__ \\ ",
    b" ___/ / / /_/ / /_/ / /_/ /___/ / ",
    b"/____/_/\\____/ .___/\\____//____/  ",
    b"            /_/                   ",
];

const VERSION: &[u8] = b"v0.2-slop";
const ARCH: &[u8] = b"x86_64";

fn write_u64_into(value: u64, buf: &mut [u8]) -> usize {
    if value == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut rev = [0u8; 20];
    let mut n = value;
    let mut r = 0usize;
    while n != 0 && r < rev.len() {
        rev[r] = b'0' + (n % 10) as u8;
        n /= 10;
        r += 1;
    }
    let mut pos = 0usize;
    while r > 0 && pos < buf.len() {
        buf[pos] = rev[r - 1];
        pos += 1;
        r -= 1;
    }
    pos
}

fn write_i64_into(value: i64, buf: &mut [u8]) -> usize {
    let mut pos = 0usize;
    let abs = if value < 0 {
        if pos < buf.len() {
            buf[pos] = b'-';
            pos += 1;
        }
        (value as u64).wrapping_neg()
    } else {
        value as u64
    };
    pos += write_u64_into(abs, &mut buf[pos..]);
    pos
}

fn format_uptime(ms: u64, buf: &mut [u8]) -> usize {
    let total_secs = (ms / 1000) as u32;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;

    let mut pos = 0usize;
    if h > 0 {
        pos += write_u64_into(h as u64, &mut buf[pos..]);
        buf[pos] = b'h';
        pos += 1;
        buf[pos] = b' ';
        pos += 1;
    }
    pos += write_u64_into(m as u64, &mut buf[pos..]);
    buf[pos] = b'm';
    pos += 1;
    buf[pos] = b' ';
    pos += 1;
    pos += write_u64_into(s as u64, &mut buf[pos..]);
    buf[pos] = b's';
    pos += 1;
    pos
}

pub fn print_welcome_banner() {
    shell_write(b"\n");

    for line in LOGO {
        shell_write_idx(line, COLOR_PROMPT_ACCENT);
        shell_write(b"\n");
    }

    shell_write(b"\n");

    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);
    shell_write_idx(VERSION, COLOR_EXEC_GREEN);
    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);
    shell_write_idx(ARCH, COLOR_COMMENT_GRAY);
    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);

    let uptime_ms = sys_core::get_time_ms();
    let mut uptime_buf = [0u8; 32];
    let uptime_len = format_uptime(uptime_ms, &mut uptime_buf);
    shell_write_idx(b"up ", COLOR_COMMENT_GRAY);
    shell_write_idx(&uptime_buf[..uptime_len], COLOR_COMMENT_GRAY);

    shell_write(b"\n");

    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) == 0 {
        let balance = info.wl_balance;

        shell_write_idx(b"  W/L: ", COLOR_COMMENT_GRAY);

        let mut bal_buf = [0u8; 21];
        let bal_len = write_i64_into(balance, &mut bal_buf);

        let bal_color = if balance > 0 {
            COLOR_EXEC_GREEN
        } else if balance < 0 {
            COLOR_ERROR_RED
        } else {
            COLOR_WARN_YELLOW
        };
        shell_write_idx(&bal_buf[..bal_len], bal_color);

        let fate_msg: &[u8] = if balance > 100 {
            b"  The Wheel favors the bold."
        } else if balance > 0 {
            b"  Fate watches with interest."
        } else if balance == 0 {
            b"  Perfectly balanced."
        } else if balance > -100 {
            b"  The house is winning."
        } else {
            b"  Deep in the red."
        };
        shell_write_idx(fate_msg, COLOR_COMMENT_GRAY);
        shell_write(b"\n");
    }

    shell_write(b"\n");
}
