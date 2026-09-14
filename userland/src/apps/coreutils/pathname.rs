//! Path arithmetic and command lookup: `basename`, `dirname`, `which`.

use super::input::as_str;
use super::opts::{Opt, Opts};
use super::{Ctx, Tool, fsutil};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "basename",
        desc: "Strip directory and suffix from a path",
        usage: "basename string [suffix]",
        run: basename,
    },
    Tool {
        name: "dirname",
        desc: "Strip the last component from a path",
        usage: "dirname string",
        run: dirname,
    },
    Tool {
        name: "which",
        desc: "Locate a command on PATH",
        usage: "which [-a] name...",
        run: which,
    },
];

fn basename(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let operands = &argv[1..];
    if operands.is_empty() || operands.len() > 2 {
        return ctx.usage("basename string [suffix]");
    }
    let Some(path) = as_str(ctx, operands[0]) else {
        return 1;
    };
    let mut name = fsutil::base_name(path);
    if let Some(suffix) = operands.get(1) {
        // POSIX: the suffix is not removed when it is the whole name.
        if let Ok(text) = core::str::from_utf8(suffix)
            && name.len() > text.len()
            && name.ends_with(text)
        {
            name = &name[..name.len() - text.len()];
        }
    }
    ctx.out.s(name);
    ctx.out.nl();
    0
}

fn dirname(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let operands = &argv[1..];
    if operands.len() != 1 {
        return ctx.usage("dirname string");
    }
    let Some(path) = as_str(ctx, operands[0]) else {
        return 1;
    };
    ctx.out.s(fsutil::dir_name(path));
    ctx.out.nl();
    0
}

/// `which` answers from `PATH` alone. The shell's builtins and its `type` are
/// a different question — this is the one a build script asks, and it must
/// agree with what `exec` would find.
fn which(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut all = false;
    let mut opts = Opts::new(argv, "a");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'a') => all = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage("which [-a] name...");
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage("which [-a] name...");
    }

    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/bin:/sbin".to_string());
    let mut status = 0;
    for operand in operands {
        let Some(name) = as_str(ctx, operand) else {
            status = 2;
            continue;
        };
        // A name with a slash is not looked up, as `exec` does not look it up.
        if name.contains('/') {
            if is_executable(name) {
                ctx.out.s(name);
                ctx.out.nl();
            } else {
                status = 1;
            }
            continue;
        }
        let mut found = false;
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = fsutil::join(dir, name);
            if is_executable(&candidate) {
                ctx.out.s(&candidate);
                ctx.out.nl();
                found = true;
                if !all {
                    break;
                }
            }
        }
        if !found {
            status = 1;
        }
    }
    status
}

/// A regular file is executable here: SlopOS is single-user at uid 0 and the
/// exec permission bit is not consulted by the loader, so claiming otherwise
/// would answer a question the kernel does not ask.
fn is_executable(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}
