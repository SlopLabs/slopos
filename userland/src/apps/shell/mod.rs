use core::cell::UnsafeCell;
use core::ffi::c_void;

pub mod buffers;
pub mod builtins;
pub mod display;
pub mod input;
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

// Safety: Userland is single-threaded with no preemption during shell code
unsafe impl<T> Sync for SyncUnsafeCell<T> {}

static PROMPT: &[u8] = b"$ ";
static NL: &[u8] = b"\n";
static WELCOME: &[u8] = b"SlopOS Shell v0.1 (userland)\n";
static UNKNOWN_CMD: &[u8] = b"Unknown command. Type 'help'.\n";
static PATH_TOO_LONG: &[u8] = b"path too long\n";
static ERR_NO_SUCH: &[u8] = b"No such file or directory\n";
static ERR_TOO_MANY_ARGS: &[u8] = b"too many arguments\n";
static ERR_MISSING_OPERAND: &[u8] = b"missing operand\n";
static ERR_MISSING_FILE: &[u8] = b"missing file operand\n";
static ERR_MISSING_TEXT: &[u8] = b"missing text operand\n";
static HALTED: &[u8] = b"Shell requested shutdown...\n";
static REBOOTING: &[u8] = b"Shell requested reboot...\n";
static HELP_HEADER: &[u8] = b"Available commands:\n";

const SHELL_IO_MAX: usize = 512;

pub struct ShellState {
    // Phase 0: just wiring
    // Phase 1: add history, input_state
    // Phase 2: add job_table
    // Phase 3: add env
}

pub fn shell_user_main(_arg: *mut c_void) {
    use crate::syscall::window;
    use display::shell_write;

    display::shell_console_init();
    display::shell_console_clear();

    window::surface_set_title("SlopOS Shell");

    shell_write(WELCOME);

    loop {
        shell_write(PROMPT);

        let mut tokens = [core::ptr::null(); parser::SHELL_MAX_TOKENS];
        let token_count = input::read_command_line(&mut tokens);

        if token_count <= 0 {
            continue;
        }

        if let Some(b) = builtins::find_builtin(tokens[0]) {
            (b.func)(token_count, &tokens);
        } else {
            shell_write(UNKNOWN_CMD);
        }
    }
}
