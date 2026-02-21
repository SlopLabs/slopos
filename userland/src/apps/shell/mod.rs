use core::cell::UnsafeCell;
use core::ffi::c_void;

pub mod buffers;
pub mod builtins;
pub mod completion;
pub mod display;
pub mod env;
pub mod exec;
pub mod history;
pub mod input;
pub mod jobs;
pub mod parser;
mod surface;

#[repr(transparent)]
pub(crate) struct SyncUnsafeCell<T>(UnsafeCell<T>);

impl<T> SyncUnsafeCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    #[inline]
    pub(crate) fn get(&self) -> *mut T {
        self.0.get()
    }
}

unsafe impl<T> Sync for SyncUnsafeCell<T> {}

pub(crate) static NL: &[u8] = b"\n";
static WELCOME: &[u8] = b"SlopOS Shell v0.2 (userland)\n";
pub(crate) static UNKNOWN_CMD: &[u8] = b"Unknown command. Type 'help'.\n";
pub(crate) static PATH_TOO_LONG: &[u8] = b"path too long\n";
pub(crate) static ERR_NO_SUCH: &[u8] = b"No such file or directory\n";
pub(crate) static ERR_TOO_MANY_ARGS: &[u8] = b"too many arguments\n";
pub(crate) static ERR_MISSING_OPERAND: &[u8] = b"missing operand\n";
pub(crate) static ERR_MISSING_FILE: &[u8] = b"missing file operand\n";
pub(crate) static ERR_MISSING_TEXT: &[u8] = b"missing text operand\n";
pub(crate) static HALTED: &[u8] = b"Shell requested shutdown...\n";
pub(crate) static REBOOTING: &[u8] = b"Shell requested reboot...\n";

pub(crate) const SHELL_IO_MAX: usize = 512;

const CWD_MAX: usize = 256;
static CWD: SyncUnsafeCell<[u8; CWD_MAX]> = SyncUnsafeCell::new([0; CWD_MAX]);

static LAST_EXIT_CODE: SyncUnsafeCell<i32> = SyncUnsafeCell::new(0);
static LAST_BG_PID: SyncUnsafeCell<u32> = SyncUnsafeCell::new(0);
static SHELL_PID: SyncUnsafeCell<u32> = SyncUnsafeCell::new(0);

pub fn cwd_bytes() -> [u8; CWD_MAX] {
    unsafe { *CWD.get() }
}

pub fn cwd_set(path: &[u8]) {
    let cwd = unsafe { &mut *CWD.get() };
    let len = path.len().min(CWD_MAX - 1);
    cwd[..len].copy_from_slice(&path[..len]);
    cwd[len] = 0;
}

pub fn last_exit_code() -> i32 {
    unsafe { *LAST_EXIT_CODE.get() }
}

pub fn set_last_exit_code(code: i32) {
    unsafe { *LAST_EXIT_CODE.get() = code }
}

pub fn last_bg_pid() -> u32 {
    unsafe { *LAST_BG_PID.get() }
}

pub fn set_last_bg_pid(pid: u32) {
    unsafe { *LAST_BG_PID.get() = pid }
}

pub fn shell_pid() -> u32 {
    unsafe { *SHELL_PID.get() }
}

fn build_prompt(buf: &mut [u8; 280]) -> usize {
    let cwd = unsafe { &*CWD.get() };
    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(0);
    let mut pos = 0;

    buf[pos] = b'[';
    pos += 1;

    let copy_len = cwd_len.min(buf.len() - 5);
    buf[pos..pos + copy_len].copy_from_slice(&cwd[..copy_len]);
    pos += copy_len;

    buf[pos] = b']';
    pos += 1;
    buf[pos] = b' ';
    pos += 1;
    buf[pos] = b'$';
    pos += 1;
    buf[pos] = b' ';
    pos += 1;

    pos
}

/// Color indices for `[/path] $ `: brackets+path in PATH_BLUE, `$` in PROMPT_ACCENT.
fn build_prompt_colors(buf: &mut [u8; 280], prompt_len: usize) {
    use display::{COLOR_DEFAULT, COLOR_PATH_BLUE, COLOR_PROMPT_ACCENT};

    let cwd = unsafe { &*CWD.get() };
    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(0);
    let bracket_and_path = 1 + cwd_len.min(275) + 1;

    for i in 0..prompt_len.min(280) {
        buf[i] = if i < bracket_and_path {
            COLOR_PATH_BLUE
        } else if i == bracket_and_path + 1 {
            COLOR_PROMPT_ACCENT
        } else {
            COLOR_DEFAULT
        };
    }
}

fn write_colored_prompt(prompt: &[u8], colors: &[u8]) {
    use display::{COLOR_DEFAULT, shell_console_commit, shell_console_write_colored};

    let _ = crate::syscall::tty::write(prompt);

    let mut i = 0;
    while i < prompt.len() {
        let color = if i < colors.len() {
            colors[i]
        } else {
            COLOR_DEFAULT
        };
        let start = i;
        while i < prompt.len() && (i >= colors.len() || colors[i] == color) {
            i += 1;
        }
        shell_console_write_colored(&prompt[start..i], color);
    }
    shell_console_commit();
}

pub struct ShellState {
    pub prompt_buf: [u8; 280],
    pub prompt_colors: [u8; 280],
    pub prompt_len: usize,
}

pub fn shell_user_main(_arg: *mut c_void) {
    use slopos_abi::signal::SIGINT;

    use crate::syscall::process;
    use crate::syscall::window;
    use display::shell_write;

    display::shell_console_init();
    display::shell_console_clear();

    window::surface_set_title("SlopOS Shell");

    cwd_set(b"/");
    env::initialize_defaults();
    unsafe { *SHELL_PID.get() = process::getpid() }
    exec::initialize_job_control();
    let _ = process::ignore_signal(SIGINT);

    shell_write(WELCOME);

    let mut state = ShellState {
        prompt_buf: [0; 280],
        prompt_colors: [0; 280],
        prompt_len: 0,
    };

    loop {
        jobs::notify_completed_jobs();
        state.prompt_len = build_prompt(&mut state.prompt_buf);
        build_prompt_colors(&mut state.prompt_colors, state.prompt_len);
        let prompt = &state.prompt_buf[..state.prompt_len];

        write_colored_prompt(prompt, &state.prompt_colors[..state.prompt_len]);

        let mut tokens = [core::ptr::null(); parser::SHELL_MAX_TOKENS];
        let prompt_colors = &state.prompt_colors[..state.prompt_len];
        let token_count = input::read_command_line(&mut tokens, prompt, prompt_colors);

        if token_count <= 0 {
            continue;
        }

        let rc = exec::execute_tokens(token_count, &tokens);
        set_last_exit_code(rc);
        if rc == 127 {
            display::shell_write_idx(UNKNOWN_CMD, display::COLOR_ERROR_RED);
        }
    }
}
