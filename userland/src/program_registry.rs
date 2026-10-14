#[derive(Clone, Copy)]
pub struct ProgramSpec {
    pub name: &'static str,
    pub path: &'static str,
    pub desc: &'static str,
}

const PROGRAM_REGISTRY: &[ProgramSpec] = &[
    ProgramSpec {
        name: "keymap",
        path: "/bin/keymap",
        desc: "Query or switch the keyboard layout",
    },
    ProgramSpec {
        name: "roulette",
        path: "/bin/roulette",
        desc: "Spin the Wheel of Fate",
    },
    ProgramSpec {
        name: "halt",
        path: "/bin/halt",
        desc: "Halt or reboot the machine",
    },
    ProgramSpec {
        name: "bootctl",
        path: "/bin/bootctl",
        desc: "Install a kernel into a boot slot and choose what boots",
    },
    ProgramSpec {
        name: "installer",
        path: "/bin/installer",
        desc: "Install SlopOS onto a disk from the live medium",
    },
    ProgramSpec {
        name: "editor",
        path: "/bin/editor",
        desc: "Edit text and source files",
    },
    ProgramSpec {
        name: "file_manager",
        path: "/bin/file_manager",
        desc: "Browse filesystem",
    },
    ProgramSpec {
        name: "image_viewer",
        path: "/bin/image_viewer",
        desc: "View PNG images",
    },
    ProgramSpec {
        name: "sysmon",
        path: "/bin/sysmon",
        desc: "System Monitor",
    },
    ProgramSpec {
        name: "nmap",
        path: "/bin/nmap",
        desc: "Scan network for hosts",
    },
    ProgramSpec {
        name: "ip",
        path: "/bin/ip",
        desc: "Show and configure networking",
    },
    ProgramSpec {
        name: "ss",
        path: "/bin/ss",
        desc: "Show socket statistics",
    },
    ProgramSpec {
        name: "cpufreq",
        path: "/bin/cpufreq",
        desc: "Show CPU frequency and core types; set the HWP policy",
    },
    ProgramSpec {
        name: "kconsole",
        path: "/bin/kconsole",
        desc: "Run the diagnostic console's informational commands",
    },
    ProgramSpec {
        name: "prof",
        path: "/bin/prof",
        desc: "Start, stop or report the kernel's sampling profiler",
    },
    // Aliases below: the shell passes the typed name through as `argv[0]`, so
    // one binary renders whichever name was asked for. A canonical entry must
    // precede its aliases — `resolve_program_path` returns the first match, so
    // a lookup by path never reports an alias.
    ProgramSpec {
        name: "netstat",
        path: "/bin/ss",
        desc: "Show sockets, routes and interface counters",
    },
    ProgramSpec {
        name: "ifconfig",
        path: "/bin/ip",
        desc: "Show network configuration (alias for `ip addr`)",
    },
    ProgramSpec {
        name: "nc",
        path: "/bin/nc",
        desc: "Network Swiss army knife",
    },
    ProgramSpec {
        name: "curl",
        path: "/bin/curl",
        desc: "Transfer data from URLs",
    },
    ProgramSpec {
        name: "ping",
        path: "/bin/ping",
        desc: "Send ICMP ECHO_REQUEST to network hosts",
    },
];

fn trim_nul_bytes(text: &str) -> &str {
    text.split('\0').next().unwrap_or(text)
}

pub fn resolve_program(name: &str) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(name);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.name) == requested)
}

pub fn resolve_program_path(path: &str) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(path);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.path) == requested)
}

pub fn user_programs() -> impl Iterator<Item = &'static ProgramSpec> {
    PROGRAM_REGISTRY.iter()
}
