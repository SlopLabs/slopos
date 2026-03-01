use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_USER_MODE};

#[derive(Clone, Copy)]
pub struct ProgramSpec {
    pub name: &'static [u8],
    pub path: &'static [u8],
    pub priority: u8,
    pub flags: u16,
    pub desc: &'static [u8],
    /// If true, the program owns a display surface and should be spawned
    /// directly via `spawn_path_with_attrs`. Text programs (gui=false) fall
    /// through to the fork+execve pipeline so stdout is properly captured.
    pub gui: bool,
}

const PROGRAM_REGISTRY: &[ProgramSpec] = &[
    ProgramSpec {
        name: b"init",
        path: b"/sbin/init",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"",
        gui: false,
    },
    ProgramSpec {
        name: b"shell",
        path: b"/bin/shell",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"",
        gui: false,
    },
    ProgramSpec {
        name: b"compositor",
        path: b"/bin/compositor",
        priority: 4,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR,
        desc: b"",
        gui: true,
    },
    ProgramSpec {
        name: b"roulette",
        path: b"/bin/roulette",
        priority: 5,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_DISPLAY_EXCLUSIVE,
        desc: b"Spin the Wheel of Fate",
        gui: true,
    },
    ProgramSpec {
        name: b"file_manager",
        path: b"/bin/file_manager",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"Browse filesystem",
        gui: true,
    },
    ProgramSpec {
        name: b"sysinfo",
        path: b"/bin/sysinfo",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"System information panel",
        gui: true,
    },
    ProgramSpec {
        name: b"nmap",
        path: b"/bin/nmap",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"Scan network for hosts",
        gui: false,
    },
    ProgramSpec {
        name: b"ifconfig",
        path: b"/bin/ifconfig",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"Show network configuration",
        gui: false,
    },
    ProgramSpec {
        name: b"nc",
        path: b"/bin/nc",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"Network Swiss army knife",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: b"fork_test",
        path: b"/bin/fork_test",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: b"",
        gui: false,
    },
];

fn trim_nul_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..len]
}

pub fn resolve_program(name: &[u8]) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(name);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.name) == requested)
}

pub fn resolve_program_path(path: &[u8]) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(path);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.path) == requested)
}

pub fn user_programs() -> impl Iterator<Item = &'static ProgramSpec> {
    PROGRAM_REGISTRY.iter().filter(|spec| !spec.desc.is_empty())
}
