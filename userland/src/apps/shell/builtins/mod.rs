//! Builtin command dispatch table and helpers.

pub mod env;
pub mod fs;
pub mod process;
pub mod system;

use super::display::shell_write;
use super::parser::u_streq_slice;

pub type BuiltinFn = fn(argc: i32, argv: &[*const u8]) -> i32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCategory {
    System,
    Filesystem,
    Process,
    Environment,
}

impl BuiltinCategory {
    pub const ALL: &[BuiltinCategory] = &[
        BuiltinCategory::System,
        BuiltinCategory::Filesystem,
        BuiltinCategory::Process,
        BuiltinCategory::Environment,
    ];

    pub fn label(self) -> &'static [u8] {
        match self {
            BuiltinCategory::System => b"System",
            BuiltinCategory::Filesystem => b"Filesystem",
            BuiltinCategory::Process => b"Process Control",
            BuiltinCategory::Environment => b"Environment",
        }
    }
}

pub struct BuiltinEntry {
    pub name: &'static [u8],
    pub desc: &'static [u8],
    pub usage: &'static [u8],
    pub detail: &'static [u8],
    pub category: BuiltinCategory,
    pub func: BuiltinFn,
}

use BuiltinCategory::*;

pub static BUILTINS: &[BuiltinEntry] = &[
    // ── System ──────────────────────────────────────────────────────────────
    BuiltinEntry {
        name: b"help",
        desc: b"Show this help",
        usage: b"help [command]",
        detail: b"Display a categorized list of all shell builtins.\nProvide a command name to see its detailed usage.",
        category: System,
        func: system::cmd_help,
    },
    BuiltinEntry {
        name: b"echo",
        desc: b"Print arguments to terminal",
        usage: b"echo [args...]",
        detail: b"Write each argument to standard output separated by\nspaces, followed by a newline.",
        category: System,
        func: system::cmd_echo,
    },
    BuiltinEntry {
        name: b"clear",
        desc: b"Clear the screen",
        usage: b"clear",
        detail: b"Reset the terminal display and move the cursor to\nthe top-left corner.",
        category: System,
        func: system::cmd_clear,
    },
    BuiltinEntry {
        name: b"info",
        desc: b"Kernel and scheduler stats",
        usage: b"info",
        detail: b"Print memory page counts, active tasks, context\nswitches, and scheduler statistics.",
        category: System,
        func: system::cmd_info,
    },
    BuiltinEntry {
        name: b"shutdown",
        desc: b"Power off the system",
        usage: b"shutdown",
        detail: b"Immediately halt the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_shutdown,
    },
    BuiltinEntry {
        name: b"reboot",
        desc: b"Reboot the system",
        usage: b"reboot",
        detail: b"Immediately restart the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_reboot,
    },
    // ── Filesystem ──────────────────────────────────────────────────────────
    BuiltinEntry {
        name: b"ls",
        desc: b"List directory contents",
        usage: b"ls [path]",
        detail: b"List files and directories at the given path.\nDirectories are marked with /, files show name (size).\nEntries are sorted alphabetically. Defaults to cwd.",
        category: Filesystem,
        func: fs::cmd_ls,
    },
    BuiltinEntry {
        name: b"cat",
        desc: b"Display file contents",
        usage: b"cat [file...]",
        detail: b"Print the contents of one or more files to the\nterminal. Without arguments, reads from stdin.\nEach file is truncated at 512 bytes.",
        category: Filesystem,
        func: fs::cmd_cat,
    },
    BuiltinEntry {
        name: b"write",
        desc: b"Write text to a file",
        usage: b"write <file> <text>",
        detail: b"Create or overwrite a file with the given text.\nThe previous contents are replaced entirely.",
        category: Filesystem,
        func: fs::cmd_write,
    },
    BuiltinEntry {
        name: b"mkdir",
        desc: b"Create a directory",
        usage: b"mkdir <dir>",
        detail: b"Create a new directory at the given path.",
        category: Filesystem,
        func: fs::cmd_mkdir,
    },
    BuiltinEntry {
        name: b"rm",
        desc: b"Remove a file",
        usage: b"rm <file>",
        detail: b"Delete a file. Does not remove directories.",
        category: Filesystem,
        func: fs::cmd_rm,
    },
    BuiltinEntry {
        name: b"cd",
        desc: b"Change working directory",
        usage: b"cd [dir]",
        detail: b"Change the current working directory to dir.\nWithout arguments, returns to /.\nUse cd .. to go up one level.",
        category: Filesystem,
        func: fs::cmd_cd,
    },
    BuiltinEntry {
        name: b"pwd",
        desc: b"Print working directory",
        usage: b"pwd",
        detail: b"Print the absolute path of the current working\ndirectory.",
        category: Filesystem,
        func: fs::cmd_pwd,
    },
    BuiltinEntry {
        name: b"stat",
        desc: b"Show file information",
        usage: b"stat <path>",
        detail: b"Display file type and size for the given path.",
        category: Filesystem,
        func: fs::cmd_stat,
    },
    BuiltinEntry {
        name: b"touch",
        desc: b"Create empty file",
        usage: b"touch <path...>",
        detail: b"Create an empty file at each given path. If the\nfile already exists, it is left unchanged.",
        category: Filesystem,
        func: fs::cmd_touch,
    },
    BuiltinEntry {
        name: b"cp",
        desc: b"Copy a file",
        usage: b"cp <src> <dst>",
        detail: b"Copy the contents of src to dst. Overwrites dst\nif it exists. Does not copy directories.",
        category: Filesystem,
        func: fs::cmd_cp,
    },
    BuiltinEntry {
        name: b"mv",
        desc: b"Move a file",
        usage: b"mv <src> <dst>",
        detail: b"Move src to dst (copy then remove). Overwrites\ndst if it exists. Does not move directories.",
        category: Filesystem,
        func: fs::cmd_mv,
    },
    BuiltinEntry {
        name: b"head",
        desc: b"Show first lines of file",
        usage: b"head <file> [n]",
        detail: b"Print the first N lines of a file (default 10).",
        category: Filesystem,
        func: fs::cmd_head,
    },
    BuiltinEntry {
        name: b"tail",
        desc: b"Show last lines of file",
        usage: b"tail <file> [n]",
        detail: b"Print the last N lines of a file (default 10).\nBuffers up to 4096 bytes from the file.",
        category: Filesystem,
        func: fs::cmd_tail,
    },
    BuiltinEntry {
        name: b"wc",
        desc: b"Count lines, words, chars",
        usage: b"wc [file...]",
        detail: b"Count lines, words, and characters in each file.\nWithout arguments, reads from standard input.\nWith multiple files, prints a total line.",
        category: Filesystem,
        func: fs::cmd_wc,
    },
    BuiltinEntry {
        name: b"hexdump",
        desc: b"Hex and ASCII dump",
        usage: b"hexdump <file> [n]",
        detail: b"Display the first N bytes of a file in hexadecimal\nand ASCII (default 256, max 512).",
        category: Filesystem,
        func: fs::cmd_hexdump,
    },
    BuiltinEntry {
        name: b"diff",
        desc: b"Compare two files",
        usage: b"diff <file1> <file2>",
        detail: b"Compare two files line by line. Show differing\nlines with < and > markers. Returns 0 if files\nare identical, 1 if they differ.",
        category: Filesystem,
        func: fs::cmd_diff,
    },
    // ── Process Control ─────────────────────────────────────────────────────
    BuiltinEntry {
        name: b"jobs",
        desc: b"List background jobs",
        usage: b"jobs",
        detail: b"Show all active background jobs with their job\nnumber, process ID, and current status.",
        category: Process,
        func: process::cmd_jobs,
    },
    BuiltinEntry {
        name: b"fg",
        desc: b"Bring job to foreground",
        usage: b"fg <%job>",
        detail: b"Resume a stopped or background job in the\nforeground. Specify the job with %N notation\n(e.g. fg %1).",
        category: Process,
        func: process::cmd_fg,
    },
    BuiltinEntry {
        name: b"bg",
        desc: b"Resume a stopped job",
        usage: b"bg <%job>",
        detail: b"Continue a stopped job in the background.\nSpecify the job with %N notation (e.g. bg %1).",
        category: Process,
        func: process::cmd_bg,
    },
    BuiltinEntry {
        name: b"kill",
        desc: b"Send signal to process",
        usage: b"kill <pid | %job>",
        detail: b"Send SIGKILL to a process by PID or to a job\ngroup by %N notation (e.g. kill %1 or kill 42).",
        category: Process,
        func: process::cmd_kill,
    },
    BuiltinEntry {
        name: b"ps",
        desc: b"Show running processes",
        usage: b"ps",
        detail: b"Display task counts (total, active, ready) and\nlist windowed processes with their PID, state,\nand title.",
        category: Process,
        func: process::cmd_ps,
    },
    BuiltinEntry {
        name: b"wait",
        desc: b"Wait for process to exit",
        usage: b"wait <pid>",
        detail: b"Block the shell until the process with the given\nPID exits. Returns that process's exit status.",
        category: Process,
        func: process::cmd_wait,
    },
    BuiltinEntry {
        name: b"exec",
        desc: b"Replace shell with program",
        usage: b"exec <path>",
        detail: b"Replace the current shell process with the program\nat the given path. Does not return on success.",
        category: Process,
        func: process::cmd_exec,
    },
    // ── Environment ─────────────────────────────────────────────────────────
    BuiltinEntry {
        name: b"export",
        desc: b"Set environment variable",
        usage: b"export [KEY=VALUE...]",
        detail: b"Set one or more environment variables.\nWithout arguments, print all exported variables.",
        category: Environment,
        func: env::cmd_export,
    },
    BuiltinEntry {
        name: b"unset",
        desc: b"Remove environment variable",
        usage: b"unset <KEY...>",
        detail: b"Remove one or more variables from the environment.",
        category: Environment,
        func: env::cmd_unset,
    },
    BuiltinEntry {
        name: b"env",
        desc: b"List environment variables",
        usage: b"env",
        detail: b"Print all environment variables in KEY=VALUE format.",
        category: Environment,
        func: env::cmd_env,
    },
    BuiltinEntry {
        name: b"set",
        desc: b"Show or set shell variables",
        usage: b"set [KEY=VALUE...]",
        detail: b"Set shell variables or, without arguments, list\nall current variables.",
        category: Environment,
        func: env::cmd_set,
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
