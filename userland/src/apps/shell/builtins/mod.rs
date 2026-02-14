//! Builtin command dispatch table and helpers.

pub mod fs;
pub mod process;
pub mod system;

use super::display::shell_write;
use super::parser::u_streq_slice;

pub type BuiltinFn = fn(argc: i32, argv: &[*const u8]) -> i32;

pub struct BuiltinEntry {
    pub name: &'static [u8],
    pub desc: &'static [u8],
    pub func: BuiltinFn,
}

pub static BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: b"help",
        func: system::cmd_help,
        desc: b"List available commands",
    },
    BuiltinEntry {
        name: b"echo",
        func: system::cmd_echo,
        desc: b"Print arguments back to the terminal",
    },
    BuiltinEntry {
        name: b"clear",
        func: system::cmd_clear,
        desc: b"Clear the terminal display",
    },
    BuiltinEntry {
        name: b"shutdown",
        func: system::cmd_shutdown,
        desc: b"Power off the system",
    },
    BuiltinEntry {
        name: b"reboot",
        func: system::cmd_reboot,
        desc: b"Reboot the system",
    },
    BuiltinEntry {
        name: b"info",
        func: system::cmd_info,
        desc: b"Show kernel memory and scheduler stats",
    },
    BuiltinEntry {
        name: b"ls",
        func: fs::cmd_ls,
        desc: b"List directory contents",
    },
    BuiltinEntry {
        name: b"cat",
        func: fs::cmd_cat,
        desc: b"Display file contents",
    },
    BuiltinEntry {
        name: b"write",
        func: fs::cmd_write,
        desc: b"Write text to a file",
    },
    BuiltinEntry {
        name: b"mkdir",
        func: fs::cmd_mkdir,
        desc: b"Create a directory",
    },
    BuiltinEntry {
        name: b"rm",
        func: fs::cmd_rm,
        desc: b"Remove a file",
    },
    BuiltinEntry {
        name: b"cd",
        func: fs::cmd_cd,
        desc: b"Change working directory",
    },
    BuiltinEntry {
        name: b"pwd",
        func: fs::cmd_pwd,
        desc: b"Print working directory",
    },
    BuiltinEntry {
        name: b"jobs",
        func: process::cmd_jobs,
        desc: b"List background jobs",
    },
    BuiltinEntry {
        name: b"fg",
        func: process::cmd_fg,
        desc: b"Bring a job to foreground",
    },
    BuiltinEntry {
        name: b"bg",
        func: process::cmd_bg,
        desc: b"Resume a stopped job",
    },
    BuiltinEntry {
        name: b"kill",
        func: process::cmd_kill,
        desc: b"Send signal to pid or %job",
    },
    BuiltinEntry {
        name: b"ps",
        func: process::cmd_ps,
        desc: b"Show process counters",
    },
    BuiltinEntry {
        name: b"wait",
        func: process::cmd_wait,
        desc: b"Wait for a pid",
    },
    BuiltinEntry {
        name: b"exec",
        func: process::cmd_exec,
        desc: b"Replace shell with program",
    },
];

pub fn find_builtin(name: *const u8) -> Option<&'static BuiltinEntry> {
    for entry in BUILTINS {
        if u_streq_slice(name, entry.name) {
            return Some(entry);
        }
    }
    None
}

pub fn print_kv(key: &[u8], value: u64) {
    if !key.is_empty() {
        shell_write(key);
    }
    let mut tmp = [0u8; 32];
    let mut idx = 0usize;
    if value == 0 {
        tmp[idx] = b'0';
        idx += 1;
    } else {
        let mut n = value;
        let mut rev = [0u8; 32];
        let mut r = 0usize;
        while n != 0 && r < rev.len() {
            rev[r] = b'0' + (n % 10) as u8;
            n /= 10;
            r += 1;
        }
        while r > 0 && idx < tmp.len() {
            idx += 1;
            tmp[idx - 1] = rev[r - 1];
            r -= 1;
        }
    }
    shell_write(&tmp[..idx]);
    shell_write(super::NL);
}
