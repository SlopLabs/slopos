//! `xargs`: turn standard input into the operand list of a command.

use std::process::Command;

use super::fsutil;
use super::input::{Stdin, read_to_end};
use super::opts::{Opt, Opts, parse_u64};
use super::{Ctx, Tool};

const USAGE: &str = "xargs [-0rt] [-n max] [-s size] [-I replace] [command [arg...]]";

/// Half the kernel's `EXEC_MAX_ARG_PAGES` budget (32 pages, 128 KiB), leaving
/// room for the environment the child is spawned with.
const ARG_BUDGET: usize = 64 * 1024;

pub static TOOLS: &[Tool] = &[Tool {
    name: "xargs",
    desc: "Build and run command lines from standard input",
    usage: USAGE,
    run: xargs,
}];

enum Flow {
    Ok,
    Failed,
    Stop(i32),
}

fn xargs(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut nul = false;
    let mut trace = false;
    let mut skip_empty = false;
    let mut max_args = usize::MAX;
    let mut budget = ARG_BUDGET;
    let mut replace: Option<String> = None;

    let mut opts = Opts::new(argv, "0rtn:s:I:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'0') => nul = true,
            Opt::Flag(b'r') => skip_empty = true,
            Opt::Flag(b't') => trace = true,
            Opt::Value(b'n', value) => match parse_u64(value) {
                Some(n) if n > 0 => max_args = n as usize,
                _ => {
                    ctx.warn_at(value, b"invalid number of arguments");
                    return ctx.usage(USAGE);
                }
            },
            Opt::Value(b's', value) => match parse_u64(value) {
                Some(n) if n > 0 => budget = (n as usize).min(ARG_BUDGET),
                _ => {
                    ctx.warn_at(value, b"invalid argument list size");
                    return ctx.usage(USAGE);
                }
            },
            Opt::Value(b'I', value) => match std::str::from_utf8(value) {
                Ok(text) if !text.is_empty() => replace = Some(text.to_string()),
                _ => {
                    ctx.warn_at(value, b"invalid replacement string");
                    return ctx.usage(USAGE);
                }
            },
            Opt::Long(name, _) if name == b"no-run-if-empty" => skip_empty = true,
            Opt::Long(name, _) if name == b"null" => nul = true,
            Opt::Long(name, _) => {
                ctx.warn_at(name, b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }

    let operands = opts.operands();
    // With no command operand the utility is `echo`, and it is dispatched
    // in-process through `super::run` rather than spawned from `/bin`.
    let builtin = operands.is_empty();
    let mut command: Vec<String> = Vec::new();
    if builtin {
        command.push("echo".to_string());
    } else {
        for operand in operands {
            match std::str::from_utf8(operand) {
                Ok(text) => command.push(text.to_string()),
                Err(_) => {
                    ctx.warn_at(operand, b"invalid argument");
                    return 1;
                }
            }
        }
    }

    let data = match read_to_end(&mut Stdin) {
        Ok(data) => data,
        Err(error) => {
            ctx.warn_io(b"stdin", &error);
            return 1;
        }
    };
    let raw = if nul {
        split_on(&data, 0)
    } else if replace.is_some() {
        split_on(&data, b'\n')
    } else {
        split_blanks(&data)
    };
    let mut items: Vec<String> = Vec::with_capacity(raw.len());
    for item in &raw {
        match std::str::from_utf8(item) {
            Ok(text) => items.push(text.to_string()),
            Err(_) => {
                ctx.warn_at(item, b"invalid argument");
                return 1;
            }
        }
    }

    let base: usize = command.iter().map(|arg| arg.len() + 1).sum();
    let mut status = 0;

    if items.is_empty() {
        if skip_empty || replace.is_some() {
            return status;
        }
        return match run(ctx, &command, trace, builtin) {
            Flow::Ok => 0,
            Flow::Failed => 123,
            Flow::Stop(code) => code,
        };
    }

    if let Some(marker) = replace.as_deref() {
        for item in &items {
            let argv: Vec<String> = command
                .iter()
                .map(|arg| arg.replace(marker, item))
                .collect();
            match run(ctx, &argv, trace, builtin) {
                Flow::Ok => {}
                Flow::Failed => status = 123,
                Flow::Stop(code) => return code,
            }
        }
        return status;
    }

    let mut index = 0;
    while index < items.len() {
        let mut argv = command.clone();
        let mut bytes = base;
        let mut count = 0usize;
        while index < items.len() {
            let item = &items[index];
            if count > 0 && (count >= max_args || bytes + item.len() + 1 > budget) {
                break;
            }
            bytes += item.len() + 1;
            argv.push(item.clone());
            count += 1;
            index += 1;
        }
        match run(ctx, &argv, trace, builtin) {
            Flow::Ok => {}
            Flow::Failed => status = 123,
            Flow::Stop(code) => return code,
        }
    }
    status
}

/// The status contract `find | xargs` is reported through: 123 for a command
/// that failed, 124 for one that exited 255, 125 for one that was signalled,
/// 126 for one that could not be run and 127 for one that was not found.
fn run(ctx: &mut Ctx, argv: &[String], trace: bool, builtin: bool) -> Flow {
    if trace {
        for (i, arg) in argv.iter().enumerate() {
            if i > 0 {
                ctx.err.b(b' ');
            }
            ctx.err.s(arg);
        }
        ctx.err.nl();
        ctx.err.flush();
    }

    if builtin {
        let bytes: Vec<&[u8]> = argv.iter().map(|arg| arg.as_bytes()).collect();
        let tool = ctx.tool();
        let code = super::run(b"echo", &bytes, ctx);
        ctx.set_tool(tool);
        return classify(code);
    }

    // The child inherits fd 1; buffered output has to land before it writes.
    ctx.out.flush();
    let Some(program) = resolve(&argv[0]) else {
        ctx.warn_at(argv[0].as_bytes(), b"not found");
        return Flow::Stop(127);
    };
    let mut command = Command::new(&program);
    for arg in &argv[1..] {
        command.arg(arg);
    }
    match command.status() {
        Ok(status) => match status.code() {
            Some(code) => classify(code),
            None => Flow::Stop(125),
        },
        Err(error) => {
            ctx.warn_io(argv[0].as_bytes(), &error);
            Flow::Stop(126)
        }
    }
}

/// A child's own 126 or 127 is the child's business, not xargs's detection of
/// a missing command, so it does not abandon the remaining input.
fn classify(code: i32) -> Flow {
    match code {
        0 => Flow::Ok,
        255 => Flow::Stop(124),
        _ => Flow::Failed,
    }
}

fn resolve(name: &str) -> Option<String> {
    if name.contains('/') {
        return is_executable(name).then(|| name.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/sbin".to_string());
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| fsutil::join(dir, name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

fn split_on(data: &[u8], sep: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for field in data.split(|&b| b == sep) {
        let trimmed = if sep == b'\n' {
            trim_blanks(field)
        } else {
            field
        };
        if !trimmed.is_empty() {
            out.push(trimmed.to_vec());
        }
    }
    out
}

fn trim_blanks(field: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = field.len();
    while start < end && is_blank(field[start]) {
        start += 1;
    }
    while end > start && is_blank(field[end - 1]) {
        end -= 1;
    }
    &field[start..end]
}

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// POSIX word splitting for the default input format: blanks separate,
/// a quote runs to its mate verbatim, and a backslash escapes one byte.
fn split_blanks(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut started = false;
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];
        if is_blank(byte) {
            if started {
                out.push(std::mem::take(&mut current));
                started = false;
            }
            i += 1;
            continue;
        }
        match byte {
            b'\\' => {
                if i + 1 < data.len() {
                    current.push(data[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
                started = true;
            }
            b'\'' | b'"' => {
                let quote = byte;
                i += 1;
                started = true;
                while i < data.len() && data[i] != quote {
                    current.push(data[i]);
                    i += 1;
                }
                if i < data.len() {
                    i += 1;
                }
            }
            _ => {
                current.push(byte);
                started = true;
                i += 1;
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}
