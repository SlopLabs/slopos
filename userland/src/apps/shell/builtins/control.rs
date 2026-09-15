//! The builtins a script leans on: control flow, `read`, `eval`, `.`, and the
//! command-lookup pair `command`/`type`.
//!
//! `break`, `continue` and `return` change what the *executor* does next, but
//! a builtin's signature is a status, so they request a
//! [`Flow`](super::super::exec::Flow) the executor honours where it can. That
//! is also what makes `break` inside `eval` work with no second mechanism.

use super::super::display::{COLOR_ERROR_RED, shell_error_named, shell_write, shell_write_idx};
use super::super::exec::{self, Flow};
use super::super::{env, funcs, script};
use crate::syscall::fs;

fn parse_count(arg: Option<&&[u8]>, default: u32) -> Option<u32> {
    let Some(arg) = arg else { return Some(default) };
    if arg.is_empty() || !arg.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(arg.iter().fold(0u32, |acc, b| {
        acc.saturating_mul(10).saturating_add((b - b'0') as u32)
    }))
}

pub fn cmd_break(argc: i32, argv: &[&[u8]]) -> i32 {
    control_jump(argc, argv, true)
}

pub fn cmd_continue(argc: i32, argv: &[&[u8]]) -> i32 {
    control_jump(argc, argv, false)
}

fn control_jump(argc: i32, argv: &[&[u8]], is_break: bool) -> i32 {
    let name: &[u8] = if is_break { b"break" } else { b"continue" };
    let Some(levels) = parse_count(argv.get(1).filter(|_| argc > 1), 1) else {
        shell_error_named(name, b"numeric argument required");
        return 2;
    };
    if levels == 0 {
        shell_error_named(name, b"argument must be at least 1");
        return 2;
    }
    // POSIX leaves this unspecified, and silently unwinding a script is the
    // dangerous reading of it.
    if !exec::in_loop() {
        shell_error_named(name, b"only meaningful in a loop");
        return 0;
    }
    exec::request_flow(if is_break {
        Flow::Break(levels)
    } else {
        Flow::Continue(levels)
    });
    0
}

pub fn cmd_return(argc: i32, argv: &[&[u8]]) -> i32 {
    let Some(status) = parse_count(argv.get(1).filter(|_| argc > 1), u32::MAX) else {
        shell_error_named(b"return", b"numeric argument required");
        return 2;
    };
    if !exec::in_function() {
        shell_error_named(b"return", b"only meaningful in a function or sourced file");
        return 1;
    }
    exec::request_flow(Flow::Return);
    if status == u32::MAX {
        super::super::last_exit_code()
    } else {
        (status & 0xff) as i32
    }
}

pub fn cmd_shift(argc: i32, argv: &[&[u8]]) -> i32 {
    let Some(count) = parse_count(argv.get(1).filter(|_| argc > 1), 1) else {
        shell_error_named(b"shift", b"numeric argument required");
        return 2;
    };
    if super::super::args::shift(count as usize) {
        0
    } else {
        shell_error_named(b"shift", b"not that many parameters");
        1
    }
}

/// `:` — expand the arguments, do nothing, succeed.
pub fn cmd_colon(_argc: i32, _argv: &[&[u8]]) -> i32 {
    0
}

/// `eval word...` — join the arguments and run the result as shell input.
pub fn cmd_eval(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        return 0;
    }
    let mut text = Vec::new();
    for arg in argv.iter().take(argc as usize).skip(1) {
        if !text.is_empty() {
            text.push(b' ');
        }
        text.extend_from_slice(arg);
    }
    run_nested(&text)
}

/// `. file` / `source file` — run a file's commands in this shell.
pub fn cmd_dot(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_error_named(b".", b"filename argument required");
        return 2;
    }
    exec::in_return_scope(|| script::source_file(argv[1]))
}

/// Run shell text nested inside a builtin, passing any control flow it
/// requested back out: `eval break` must break the enclosing loop.
fn run_nested(text: &[u8]) -> i32 {
    match exec::parse_text(text) {
        Ok(list) => {
            let outcome = exec::run_nested_list(&list);
            if outcome.flow != Flow::Normal {
                exec::request_flow(outcome.flow);
            }
            outcome.status
        }
        Err(failure) => {
            exec::report_parse_failure(failure);
            exec::STATUS_SYNTAX_ERROR
        }
    }
}

/// `read [-r] [-p prompt] name...`
///
/// One byte at a time, because the descriptor is shared with whatever runs
/// next: `while read l; do ...; done < file` needs exactly its own line.
pub fn cmd_read(argc: i32, argv: &[&[u8]]) -> i32 {
    let argc = argc as usize;
    let mut raw = false;
    let mut index = 1usize;
    while index < argc {
        match argv[index] {
            b"-r" => raw = true,
            b"-p" => {
                index += 1;
                if index < argc {
                    shell_write(argv[index]);
                }
            }
            arg if arg.starts_with(b"-") && arg.len() > 1 => {
                shell_error_named(b"read", b"unknown option");
                return 2;
            }
            _ => break,
        }
        index += 1;
    }

    let names: Vec<&[u8]> = argv[index..argc].to_vec();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let mut hit_eof = true;
    loop {
        match fs::read_slice(0, &mut byte) {
            Ok(0) => break,
            Ok(_) => {
                hit_eof = false;
                if byte[0] == b'\n' {
                    break;
                }
                // An unescaped backslash continues the line, unless `-r`.
                if !raw && byte[0] == b'\\' {
                    let mut next = [0u8; 1];
                    match fs::read_slice(0, &mut next) {
                        Ok(0) => break,
                        Ok(_) if next[0] == b'\n' => continue,
                        Ok(_) => line.push(next[0]),
                        Err(e) if e == crate::syscall::SyscallError::EINTR => continue,
                        Err(_) => break,
                    }
                    continue;
                }
                line.push(byte[0]);
            }
            Err(e) if e == crate::syscall::SyscallError::EINTR => continue,
            Err(_) => break,
        }
    }

    if names.is_empty() {
        env::set(b"REPLY", &line);
        return i32::from(hit_eof);
    }

    // Split into as many fields as there are names; the last name takes the
    // whole remainder, delimiters and all, as POSIX requires.
    let ifs = env::get(b"IFS").unwrap_or_else(|| b" \t\n".to_vec());
    let mut rest = line.as_slice();
    for (position, name) in names.iter().enumerate() {
        let last = position + 1 == names.len();
        while !last && rest.first().is_some_and(|b| ifs.contains(b)) {
            rest = &rest[1..];
        }
        if last {
            let trimmed = trim_ifs(rest, &ifs);
            env::set(name, trimmed);
            break;
        }
        let end = rest
            .iter()
            .position(|b| ifs.contains(b))
            .unwrap_or(rest.len());
        env::set(name, &rest[..end]);
        rest = &rest[end..];
    }
    i32::from(hit_eof)
}

fn trim_ifs<'a>(mut bytes: &'a [u8], ifs: &[u8]) -> &'a [u8] {
    while bytes.first().is_some_and(|b| ifs.contains(b)) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|b| ifs.contains(b)) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// `command [-v|-V] name [arg...]` — run `name` ignoring any function of that
/// name, or report what it resolves to. A configure-style script probes with
/// `command -v` before anything else.
pub fn cmd_command(argc: i32, argv: &[&[u8]]) -> i32 {
    let argc = argc as usize;
    let mut index = 1usize;
    // POSIX splits the two: `-v` writes a name a shell could re-use, `-V` a
    // sentence for a human.
    let mut terse = false;
    let mut verbose = false;
    while index < argc {
        match argv[index] {
            b"-v" => terse = true,
            b"-V" => verbose = true,
            b"-p" => {}
            b"--" => {
                index += 1;
                break;
            }
            arg if arg.starts_with(b"-") && arg.len() > 1 => {
                shell_error_named(b"command", b"unknown option");
                return 2;
            }
            _ => break,
        }
        index += 1;
    }
    if index >= argc {
        return i32::from(terse || verbose);
    }

    if verbose {
        return describe(argv[index]);
    }
    if terse {
        return name_of(argv[index]);
    }

    // The point of `command` is the *non-function* meaning of a name.
    let Some(path) = exec::resolve_command_ignoring_functions(argv[index]) else {
        shell_error_named(argv[index], b"not found");
        return exec::STATUS_NOT_FOUND;
    };
    let mut tokens = super::super::buffers::ParsedTokens::new();
    if super::find_builtin(argv[index]).is_some() {
        tokens.push_token(argv[index]);
    } else {
        tokens.push_token(&path);
    }
    for arg in argv.iter().take(argc).skip(index + 1) {
        tokens.push_token(arg);
    }
    exec::execute_tokens(&tokens)
}

/// What `command -v` writes: a name for a function or builtin, a path for
/// anything a command search finds.
fn name_of(name: &[u8]) -> i32 {
    if funcs::lookup(name).is_some() || super::find_builtin(name).is_some() {
        shell_write(name);
        shell_write(b"\n");
        return 0;
    }
    match exec::resolve_command(name) {
        Some(path) => {
            shell_write(&path);
            shell_write(b"\n");
            0
        }
        None => 1,
    }
}

/// `type name...` — what each name resolves to.
pub fn cmd_type(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        return 0;
    }
    let mut status = 0;
    for name in argv.iter().take(argc as usize).skip(1) {
        if describe(name) != 0 {
            status = 1;
        }
    }
    status
}

fn describe(name: &[u8]) -> i32 {
    if funcs::lookup(name).is_some() {
        shell_write(name);
        shell_write(b" is a function\n");
        return 0;
    }
    if super::find_builtin(name).is_some() {
        shell_write(name);
        shell_write(b" is a shell builtin\n");
        return 0;
    }
    match exec::resolve_command(name) {
        Some(path) => {
            shell_write(name);
            shell_write(b" is ");
            shell_write(&path);
            shell_write(b"\n");
            0
        }
        None => {
            shell_write_idx(name, COLOR_ERROR_RED);
            shell_write_idx(b": not found\n", COLOR_ERROR_RED);
            1
        }
    }
}
