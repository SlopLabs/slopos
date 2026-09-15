//! Builtin command dispatch table and helpers.
//!
//! A builtin here is a command that changes the shell itself, or one POSIX
//! resolves without a fork. Every file and text utility is an executable in
//! `apps::coreutils`, reached through `PATH` — see [`utility`] for the six
//! that are both.

pub mod control;
pub mod env;
pub mod fs;
pub mod process;
pub mod system;
pub mod utility;
pub mod utils;

pub type BuiltinFn = fn(argc: i32, argv: &[&[u8]]) -> i32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCategory {
    System,
    Filesystem,
    Process,
    Environment,
    Network,
    Utility,
}

impl BuiltinCategory {
    pub const ALL: &[BuiltinCategory] = &[
        BuiltinCategory::System,
        BuiltinCategory::Filesystem,
        BuiltinCategory::Process,
        BuiltinCategory::Environment,
        BuiltinCategory::Network,
        BuiltinCategory::Utility,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BuiltinCategory::System => "System",
            BuiltinCategory::Filesystem => "Filesystem",
            BuiltinCategory::Process => "Process Control",
            BuiltinCategory::Environment => "Environment",
            BuiltinCategory::Network => "Network",
            BuiltinCategory::Utility => "Utility",
        }
    }
}

pub struct BuiltinEntry {
    pub name: &'static str,
    pub desc: &'static str,
    pub usage: &'static str,
    pub detail: &'static str,
    pub category: BuiltinCategory,
    pub func: BuiltinFn,
}

use BuiltinCategory::*;

pub static BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: "help",
        desc: "Show this help",
        usage: "help [command]",
        detail: "Display a categorized list of all shell builtins.\nProvide a command name to see its detailed usage.",
        category: System,
        func: system::cmd_help,
    },
    BuiltinEntry {
        name: "echo",
        desc: "Print arguments to terminal",
        usage: "echo [args...]",
        detail: "Write each argument to standard output separated by\nspaces, followed by a newline.",
        category: System,
        func: utility::cmd_echo,
    },
    BuiltinEntry {
        name: "clear",
        desc: "Clear the screen",
        usage: "clear",
        detail: "Reset the terminal display and move the cursor to\nthe top-left corner.",
        category: System,
        func: system::cmd_clear,
    },
    BuiltinEntry {
        name: "info",
        desc: "Kernel and scheduler stats",
        usage: "info",
        detail: "Print memory page counts, active tasks, context\nswitches, and scheduler statistics.",
        category: System,
        func: system::cmd_info,
    },
    BuiltinEntry {
        name: "shutdown",
        desc: "Power off the system",
        usage: "shutdown",
        detail: "Immediately halt the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_shutdown,
    },
    BuiltinEntry {
        name: "reboot",
        desc: "Reboot the system",
        usage: "reboot",
        detail: "Immediately restart the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_reboot,
    },
    BuiltinEntry {
        name: "uptime",
        desc: "Show system uptime",
        usage: "uptime",
        detail: "Display time elapsed since boot in hours, minutes,\nand seconds, plus total milliseconds.",
        category: System,
        func: system::cmd_uptime,
    },
    BuiltinEntry {
        name: "cpuinfo",
        desc: "Show CPU information",
        usage: "cpuinfo",
        detail: "Display architecture, CPU count, and which CPU the\nshell is currently running on.",
        category: System,
        func: system::cmd_cpuinfo,
    },
    BuiltinEntry {
        name: "free",
        desc: "Show memory usage",
        usage: "free",
        detail: "Display memory statistics in pages, KiB, and MiB.\nShows total, free, and allocated memory.",
        category: System,
        func: system::cmd_free,
    },
    BuiltinEntry {
        name: "time",
        desc: "Time a command",
        usage: "time <command> [args...]",
        detail: "Execute a command and report wall-clock elapsed\ntime after it completes.",
        category: System,
        func: system::cmd_time,
    },
    BuiltinEntry {
        name: "write",
        desc: "Write text to a file",
        usage: "write <file> <text>",
        detail: "Create or overwrite a file with the given text.\nThe previous contents are replaced entirely.",
        category: Filesystem,
        func: fs::cmd_write,
    },
    BuiltinEntry {
        name: "cd",
        desc: "Change working directory",
        usage: "cd [dir]",
        detail: "Change the current working directory to dir.\nWithout arguments, returns to /.\nUse cd .. to go up one level.",
        category: Filesystem,
        func: fs::cmd_cd,
    },
    BuiltinEntry {
        name: "pwd",
        desc: "Print working directory",
        usage: "pwd",
        detail: "Print the absolute path of the current working\ndirectory.",
        category: Filesystem,
        func: fs::cmd_pwd,
    },
    BuiltinEntry {
        name: "jobs",
        desc: "List background jobs",
        usage: "jobs",
        detail: "Show all active background jobs with their job\nnumber, process ID, and current status.",
        category: Process,
        func: process::cmd_jobs,
    },
    BuiltinEntry {
        name: "fg",
        desc: "Bring job to foreground",
        usage: "fg <%job>",
        detail: "Resume a stopped or background job in the\nforeground. Specify the job with %N notation\n(e.g. fg %1).",
        category: Process,
        func: process::cmd_fg,
    },
    BuiltinEntry {
        name: "bg",
        desc: "Resume a stopped job",
        usage: "bg <%job>",
        detail: "Continue a stopped job in the background.\nSpecify the job with %N notation (e.g. bg %1).",
        category: Process,
        func: process::cmd_bg,
    },
    BuiltinEntry {
        name: "kill",
        desc: "Send signal to process",
        usage: "kill [-SIG] <pid | %job>...",
        detail: "Send a signal to a process by PID or to a job\ngroup by %N notation. SIGTERM by default;\nname another as -9, -KILL or -s STOP.",
        category: Process,
        func: process::cmd_kill,
    },
    BuiltinEntry {
        name: "wait",
        desc: "Wait for process to exit",
        usage: "wait <pid>",
        detail: "Block the shell until the process with the given\nPID exits. Returns that process's exit status.",
        category: Process,
        func: process::cmd_wait,
    },
    BuiltinEntry {
        name: "exec",
        desc: "Replace shell with program",
        usage: "exec <path>",
        detail: "Replace the current shell process with the program\nat the given path. Does not return on success.",
        category: Process,
        func: process::cmd_exec,
    },
    BuiltinEntry {
        name: "exit",
        desc: "Exit the shell",
        usage: "exit [n]",
        detail: "End the shell with status n, or with the status of\nthe last command when n is omitted.",
        category: Process,
        func: process::cmd_exit,
    },
    BuiltinEntry {
        name: "export",
        desc: "Set environment variable",
        usage: "export [KEY=VALUE...]",
        detail: "Set one or more environment variables.\nWithout arguments, print all exported variables.",
        category: Environment,
        func: env::cmd_export,
    },
    BuiltinEntry {
        name: "unset",
        desc: "Remove environment variable",
        usage: "unset <KEY...>",
        detail: "Remove one or more variables from the environment.",
        category: Environment,
        func: env::cmd_unset,
    },
    BuiltinEntry {
        name: "env",
        desc: "List environment variables",
        usage: "env",
        detail: "Print all environment variables in KEY=VALUE format.",
        category: Environment,
        func: env::cmd_env,
    },
    BuiltinEntry {
        name: "set",
        desc: "Show or set shell variables",
        usage: "set [KEY=VALUE...]",
        detail: "Set shell variables or, without arguments, list\nall current variables.",
        category: Environment,
        func: env::cmd_set,
    },
    BuiltinEntry {
        name: "true",
        desc: "Return success",
        usage: "true",
        detail: "Do nothing and return exit code 0.",
        category: Utility,
        func: utility::cmd_true,
    },
    BuiltinEntry {
        name: "false",
        desc: "Return failure",
        usage: "false",
        detail: "Do nothing and return exit code 1.",
        category: Utility,
        func: utility::cmd_false,
    },
    BuiltinEntry {
        name: "printf",
        desc: "Format and print arguments",
        usage: "printf format [args...]",
        detail: "Write the arguments under the control of format.\nThe format is reused until the arguments are\nconsumed, so one format can print many records.",
        category: Utility,
        func: utility::cmd_printf,
    },
    BuiltinEntry {
        name: "test",
        desc: "Evaluate a conditional expression",
        usage: "test expression",
        detail: "Exit 0 when the expression is true, 1 when false.\nFile tests (-e -f -d -s), string tests (-n -z, =,\n!=) and integer tests (-eq -lt -gt ...) combine\nwith ! -a -o and parentheses.",
        category: Utility,
        func: utility::cmd_test,
    },
    BuiltinEntry {
        name: "[",
        desc: "Evaluate a conditional expression",
        usage: "[ expression ]",
        detail: "As test, but the final argument must be ].",
        category: Utility,
        func: utility::cmd_bracket,
    },
    BuiltinEntry {
        name: "random",
        desc: "Print a random number",
        usage: "random [max]",
        detail: "Print a random number. With max, prints a value\nin the range 0..max (exclusive). Without max,\nprints a raw 32-bit random value.",
        category: Utility,
        func: utils::cmd_random,
    },
    BuiltinEntry {
        name: "roulette",
        desc: "Spin the Wheel of Fate",
        usage: "roulette",
        detail: "Gamble with destiny. A win awards +10 W's.\nA loss reboots the system. The house always wins.\nEventually.",
        category: Utility,
        func: utils::cmd_roulette,
    },
    BuiltinEntry {
        name: "wl",
        desc: "Show W/L balance",
        usage: "wl",
        detail: "Display the current W/L currency balance from\nthe Wheel of Fate's ledger.",
        category: Utility,
        func: utils::cmd_wl,
    },
    BuiltinEntry {
        name: "resolve",
        desc: "Resolve hostname to IP",
        usage: "resolve <hostname>",
        detail: "Resolve a hostname to its IPv4 address using the\nin-kernel DNS client. Uses the DHCP-provided DNS\nserver (typically 10.0.2.3 on QEMU user-net).",
        category: Network,
        func: system::cmd_resolve,
    },
    BuiltinEntry {
        name: ":",
        desc: "Do nothing, successfully",
        usage: ":",
        detail: "Expand the arguments and return 0. A script uses it\nas an empty loop or branch body.",
        category: Utility,
        func: control::cmd_colon,
    },
    BuiltinEntry {
        name: "break",
        desc: "Leave an enclosing loop",
        usage: "break [n]",
        detail: "Exit the innermost enclosing for, while or until\nloop, or the nth enclosing one.",
        category: Process,
        func: control::cmd_break,
    },
    BuiltinEntry {
        name: "continue",
        desc: "Restart an enclosing loop",
        usage: "continue [n]",
        detail: "Begin the next iteration of the innermost enclosing\nloop, or of the nth enclosing one.",
        category: Process,
        func: control::cmd_continue,
    },
    BuiltinEntry {
        name: "return",
        desc: "Return from a function",
        usage: "return [n]",
        detail: "End the running function or sourced file with status\nn, or with the last command's status.",
        category: Process,
        func: control::cmd_return,
    },
    BuiltinEntry {
        name: "shift",
        desc: "Discard positional parameters",
        usage: "shift [n]",
        detail: "Drop the first n positional parameters, default 1,\nrenumbering the rest from $1.",
        category: Process,
        func: control::cmd_shift,
    },
    BuiltinEntry {
        name: "eval",
        desc: "Run arguments as a command",
        usage: "eval word...",
        detail: "Join the arguments with spaces and run the result as\nshell input in this shell.",
        category: Process,
        func: control::cmd_eval,
    },
    BuiltinEntry {
        name: ".",
        desc: "Run a file in this shell",
        usage: ". file",
        detail: "Read and run a file's commands in the current shell,\nso its variables and functions persist.",
        category: Process,
        func: control::cmd_dot,
    },
    BuiltinEntry {
        name: "source",
        desc: "Run a file in this shell",
        usage: "source file",
        detail: "As the . builtin.",
        category: Process,
        func: control::cmd_dot,
    },
    BuiltinEntry {
        name: "read",
        desc: "Read a line into variables",
        usage: "read [-r] [-p prompt] [name...]",
        detail: "Read one line from standard input and split it into\nthe named variables on IFS; the last name takes the\nremainder. Without a name, sets REPLY. -r keeps\nbackslashes literal.",
        category: Environment,
        func: control::cmd_read,
    },
    BuiltinEntry {
        name: "command",
        desc: "Run a command, ignoring functions",
        usage: "command [-v] name [arg...]",
        detail: "Run name without consulting the function table, or\nwith -v print what it resolves to.",
        category: Process,
        func: control::cmd_command,
    },
    BuiltinEntry {
        name: "type",
        desc: "Say what a name resolves to",
        usage: "type name...",
        detail: "Report each name as a function, a shell builtin, or\nthe path a command search finds.",
        category: System,
        func: control::cmd_type,
    },
];

pub fn find_builtin(name: &[u8]) -> Option<&'static BuiltinEntry> {
    for entry in BUILTINS {
        if name == entry.name.as_bytes() {
            return Some(entry);
        }
    }
    None
}
