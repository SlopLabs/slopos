use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_USER_MODE};

#[derive(Clone, Copy)]
pub struct ProgramSpec {
    pub name: &'static [u8],
    pub path: &'static [u8],
    pub priority: u8,
    pub flags: u16,
}

const PROGRAM_REGISTRY: &[ProgramSpec] = &[
    ProgramSpec {
        name: b"init",
        path: b"/sbin/init",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        name: b"shell",
        path: b"/bin/shell",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        name: b"compositor",
        path: b"/bin/compositor",
        priority: 4,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR,
    },
    ProgramSpec {
        name: b"roulette",
        path: b"/bin/roulette",
        priority: 5,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_DISPLAY_EXCLUSIVE,
    },
    ProgramSpec {
        name: b"file_manager",
        path: b"/bin/file_manager",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    ProgramSpec {
        name: b"sysinfo",
        path: b"/bin/sysinfo",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: b"fork_test",
        path: b"/bin/fork_test",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
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
