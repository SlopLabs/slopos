use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

pub mod args;
mod banner;
pub mod buffers;
pub mod builtins;
pub mod completion;
pub mod display;
pub mod env;
pub mod exec;
pub mod expand;
pub mod funcs;
pub mod glob;
pub mod history;
pub mod input;
pub mod interrupt;
pub mod jobs;
pub mod parser;
pub mod script;

pub(crate) static NL: &str = "\n";
pub(crate) static PATH_TOO_LONG: &str = "path too long\n";
pub(crate) static ERR_NO_SUCH: &str = "No such file or directory\n";
pub(crate) static ERR_TOO_MANY_ARGS: &str = "too many arguments\n";
pub(crate) static ERR_MISSING_FILE: &str = "missing file operand\n";
pub(crate) static ERR_MISSING_TEXT: &str = "missing text operand\n";
pub(crate) static HALTED: &str = "Shell requested shutdown...\n";
pub(crate) static REBOOTING: &str = "Shell requested reboot...\n";

pub(crate) const SHELL_IO_MAX: usize = 512;

/// NUL-terminated; bounded only by `USER_PATH_MAX`, so the shell adds no path
/// ceiling of its own.
static CWD: Mutex<Vec<u8>> = Mutex::new(Vec::new());

static LAST_EXIT_CODE: AtomicI32 = AtomicI32::new(0);
static LAST_BG_PID: AtomicU32 = AtomicU32::new(0);
static SHELL_PID: AtomicU32 = AtomicU32::new(0);

/// Whether this shell has a user at a terminal.  Decided once at startup.
static INTERACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_interactive() -> bool {
    INTERACTIVE.load(Ordering::Relaxed)
}

/// Set by the `exit` builtin so the command loop unwinds normally: `exit(2)`
/// from a builtin would skip the redirect restore and terminal handback its
/// caller still owes.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static EXIT_STATUS: AtomicI32 = AtomicI32::new(0);

pub fn request_exit(status: i32) {
    EXIT_STATUS.store(status, Ordering::Relaxed);
    EXIT_REQUESTED.store(true, Ordering::Relaxed);
}

pub fn exit_requested() -> Option<i32> {
    EXIT_REQUESTED
        .load(Ordering::Relaxed)
        .then(|| EXIT_STATUS.load(Ordering::Relaxed))
}

/// With its terminating NUL, and never empty: callers index `cwd[0]`.
pub fn cwd_bytes() -> Vec<u8> {
    let cwd = CWD.lock().unwrap();
    if cwd.is_empty() {
        return vec![b'/', 0];
    }
    cwd.clone()
}

pub fn cwd_set(path: &[u8]) {
    let end = path
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path.len())
        .min(slopos_abi::fs::USER_PATH_MAX - 1);
    let cwd = &mut *CWD.lock().unwrap();
    cwd.clear();
    cwd.extend_from_slice(&path[..end]);
    cwd.push(0);
}

pub fn last_exit_code() -> i32 {
    LAST_EXIT_CODE.load(Ordering::Relaxed)
}

pub fn set_last_exit_code(code: i32) {
    LAST_EXIT_CODE.store(code, Ordering::Relaxed)
}

pub fn last_bg_pid() -> u32 {
    LAST_BG_PID.load(Ordering::Relaxed)
}

pub fn set_last_bg_pid(pid: u32) {
    LAST_BG_PID.store(pid, Ordering::Relaxed)
}

pub fn shell_pid() -> u32 {
    SHELL_PID.load(Ordering::Relaxed)
}

pub(crate) const PROMPT_BUF_MAX: usize = 280;

const DEFAULT_PS1: &[u8] = b"[\\w] \\$ ";

/// Expand PS1 escape sequences (`\w` `\u` `\h` `\$` `\t` `\n` `\\`) into
/// `text_buf`/`color_buf`.
fn expand_ps1(
    ps1: &[u8],
    text_buf: &mut [u8; PROMPT_BUF_MAX],
    color_buf: &mut [u8; PROMPT_BUF_MAX],
) -> usize {
    use crate::syscall::core as sys_core;
    use crate::syscall::process;
    use display::{
        COLOR_COMMENT_GRAY, COLOR_DEFAULT, COLOR_EXEC_GREEN, COLOR_PATH_BLUE, COLOR_PROMPT_ACCENT,
    };

    let mut out = 0usize;
    let mut i = 0usize;

    while i < ps1.len() && out < PROMPT_BUF_MAX {
        if ps1[i] == b'\\' && i + 1 < ps1.len() {
            i += 1;
            match ps1[i] {
                b'w' => {
                    let cwd = CWD.lock().unwrap();
                    let cwd: &[u8] = &*cwd;
                    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(0);
                    let avail = PROMPT_BUF_MAX - out;
                    let copy = cwd_len.min(avail);
                    text_buf[out..out + copy].copy_from_slice(&cwd[..copy]);
                    fill_color(color_buf, out, copy, COLOR_PATH_BLUE);
                    out += copy;
                }
                b'u' => {
                    out += emit_segment(text_buf, color_buf, out, b"root", COLOR_EXEC_GREEN);
                }
                b'h' => {
                    out += emit_segment(text_buf, color_buf, out, b"sloptopia", COLOR_EXEC_GREEN);
                }
                b'$' => {
                    let ch = if process::getuid() == 0 { b'#' } else { b'$' };
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = ch;
                        color_buf[out] = COLOR_PROMPT_ACCENT;
                        out += 1;
                    }
                }
                b't' => {
                    let ms = sys_core::get_time_ms();
                    let total_secs = (ms / 1000) as u32;
                    let h = total_secs / 3600;
                    let m = (total_secs % 3600) / 60;
                    let s = total_secs % 60;
                    let mut time_buf = [0u8; 8];
                    time_buf[0] = b'0' + (h / 10 % 10) as u8;
                    time_buf[1] = b'0' + (h % 10) as u8;
                    time_buf[2] = b':';
                    time_buf[3] = b'0' + (m / 10) as u8;
                    time_buf[4] = b'0' + (m % 10) as u8;
                    time_buf[5] = b':';
                    time_buf[6] = b'0' + (s / 10) as u8;
                    time_buf[7] = b'0' + (s % 10) as u8;
                    out += emit_segment(text_buf, color_buf, out, &time_buf, COLOR_COMMENT_GRAY);
                }
                b'n' => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\n';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
                b'\\' => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\\';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
                other => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\\';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = other;
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
            }
            i += 1;
        } else {
            text_buf[out] = ps1[i];
            color_buf[out] = display::COLOR_DEFAULT;
            out += 1;
            i += 1;
        }
    }

    out
}

#[inline]
fn emit_segment(
    buf: &mut [u8; PROMPT_BUF_MAX],
    colors: &mut [u8; PROMPT_BUF_MAX],
    offset: usize,
    segment: &[u8],
    color_idx: u8,
) -> usize {
    let avail = PROMPT_BUF_MAX - offset;
    let copy = segment.len().min(avail);
    buf[offset..offset + copy].copy_from_slice(&segment[..copy]);
    fill_color(colors, offset, copy, color_idx);
    copy
}

#[inline]
fn fill_color(colors: &mut [u8; PROMPT_BUF_MAX], offset: usize, count: usize, color_idx: u8) {
    let end = (offset + count).min(PROMPT_BUF_MAX);
    for slot in &mut colors[offset..end] {
        *slot = color_idx;
    }
}

fn build_prompt(
    text_buf: &mut [u8; PROMPT_BUF_MAX],
    color_buf: &mut [u8; PROMPT_BUF_MAX],
) -> usize {
    match env::get(b"PS1") {
        Some(ps1) => expand_ps1(&ps1, text_buf, color_buf),
        None => expand_ps1(DEFAULT_PS1, text_buf, color_buf),
    }
}

fn write_colored_prompt(prompt: &[u8], colors: &[u8]) {
    use display::{COLOR_DEFAULT, shell_write_idx};

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
        shell_write_idx(&prompt[start..i], color);
    }
}

pub struct ShellState {
    pub prompt_buf: [u8; PROMPT_BUF_MAX],
    pub prompt_colors: [u8; PROMPT_BUF_MAX],
    pub prompt_len: usize,
}

pub fn shell_user_main(argv: &[&str]) -> i32 {
    let invocation = match args::parse(argv) {
        Ok(inv) => inv,
        Err(args::UsageError(msg)) => {
            display::shell_error_named(b"usage", msg.as_bytes());
            return exec::STATUS_SYNTAX_ERROR;
        }
    };

    // POSIX interactivity rule: stdin decides whether a user is typing, stderr
    // whether there is anywhere to complain to; stdout is not consulted, so
    // `shell > log` typed at a terminal still prompts.
    let interactive = invocation.force_interactive
        || (matches!(invocation.source, args::Source::Stdin)
            && crate::syscall::fs::isatty(0)
            && crate::syscall::fs::isatty(2));
    INTERACTIVE.store(interactive, Ordering::Relaxed);

    cwd_set(b"/");
    env::initialize_defaults();
    SHELL_PID.store(std::process::id(), Ordering::Relaxed);
    exec::initialize_job_control();

    if interactive {
        // Only interactive: a non-interactive shell leaves SIGINT at SIG_DFL,
        // since nothing in the script loop ever polls the recorded flag.
        interrupt::install();
        return shell_interactive_main();
    }

    display::set_plain_output(true);
    match invocation.source {
        args::Source::CommandString(text) => script::run_command_string(&text),
        args::Source::File(path) => script::run_script_file(&path),
        args::Source::Stdin => script::run_script(&mut script::FdSource::new(0)),
    }
}

fn shell_interactive_main() -> i32 {
    // fd 0/1/2 are the PTY slave the parent terminal emulator provides; editing
    // rides the raw fd0 escape-sequence reader and output is ANSI to fd1.
    banner::print_welcome_banner();

    let mut state = ShellState {
        prompt_buf: [0; PROMPT_BUF_MAX],
        prompt_colors: [0; PROMPT_BUF_MAX],
        prompt_len: 0,
    };

    let mut pending: Vec<u8> = Vec::new();

    loop {
        if pending.is_empty() {
            jobs::notify_completed_jobs();
            state.prompt_len = build_prompt(&mut state.prompt_buf, &mut state.prompt_colors);
        } else {
            state.prompt_len = continuation_prompt(&mut state.prompt_buf, &mut state.prompt_colors);
        }
        let prompt = &state.prompt_buf[..state.prompt_len];
        write_colored_prompt(prompt, &state.prompt_colors[..state.prompt_len]);

        let mut line: Vec<u8> = Vec::new();
        let prompt_colors = &state.prompt_colors[..state.prompt_len];

        match input::read_command_line(&mut line, prompt, prompt_colors) {
            input::LineOutcome::Ready => {}
            input::LineOutcome::Empty => {
                if pending.is_empty() {
                    continue;
                }
            }
            input::LineOutcome::Interrupted => {
                pending.clear();
                set_last_exit_code(interrupt::EXIT_INTERRUPTED);
                continue;
            }
            input::LineOutcome::TooLong => {
                pending.clear();
                display::shell_error(b"sh: line too long\n");
                set_last_exit_code(exec::STATUS_SYNTAX_ERROR);
                continue;
            }
            // POSIX: EOF ends an interactive shell with the last command's
            // status. An unfinished command is abandoned, not guessed at.
            input::LineOutcome::Eof => {
                if !pending.is_empty() {
                    display::shell_error(b"sh: syntax error: unexpected end of input\n");
                    return exec::STATUS_SYNTAX_ERROR;
                }
                return last_exit_code();
            }
        }

        if !pending.is_empty() {
            pending.push(b'\n');
        }
        pending.extend_from_slice(&line);

        let rc = match exec::parse_text(&pending) {
            // Unfinished, not wrong: keep the text and prompt again.
            Err(exec::ParseFailure::Incomplete) => continue,
            Err(failure) => {
                pending.clear();
                exec::report_parse_failure(failure);
                exec::STATUS_SYNTAX_ERROR
            }
            Ok(list) => {
                pending.clear();
                if list.is_empty() {
                    continue;
                }
                exec::execute_list(&list)
            }
        };
        set_last_exit_code(rc);
        if let Some(status) = exit_requested() {
            return status;
        }
    }
}

/// PS2, the prompt for the rest of an unfinished command.
fn continuation_prompt(
    text_buf: &mut [u8; PROMPT_BUF_MAX],
    color_buf: &mut [u8; PROMPT_BUF_MAX],
) -> usize {
    let ps2 = env::get(b"PS2").unwrap_or_else(|| b"> ".to_vec());
    let len = ps2.len().min(PROMPT_BUF_MAX);
    text_buf[..len].copy_from_slice(&ps2[..len]);
    fill_color(color_buf, 0, len, display::COLOR_COMMENT_GRAY);
    len
}
