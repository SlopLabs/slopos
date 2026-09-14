#![feature(restricted_std)]

//! The multicall utility binary. `/bin/<tool>` is a symlink to this file, so
//! `argv[0]` names the utility; `coreutils <tool> [args]` works too, which is
//! what makes the installed set testable from one path.

use slopos_userland::apps::coreutils;

fn main() {
    let args: Vec<Vec<u8>> = std::env::args()
        .map(|arg| arg.into_bytes())
        .collect::<Vec<_>>();
    let argv: Vec<&[u8]> = args.iter().map(|arg| arg.as_slice()).collect();

    let mut ctx = coreutils::Ctx::stdio();

    // Only under its own name: a tool's `-l` is the tool's, so this must not
    // see `/bin/ls -l`. The installed symlink set is checked against `--list`,
    // which is what keeps it from drifting from the implemented table.
    let own_name = coreutils::invoked_name(&argv) == Some(b"coreutils");
    if own_name && argv.len() == 2 && argv[1] == b"--list" {
        for tool in coreutils::tools() {
            ctx.out.s(tool.name);
            ctx.out.nl();
        }
        ctx.out.flush();
        std::process::exit(0);
    }

    let Some((name, tool_argv)) = coreutils::select(&argv) else {
        ctx.set_tool("coreutils");
        ctx.err
            .s("usage: coreutils <tool> [args...]\n       coreutils --list\n");
        ctx.err.flush();
        std::process::exit(2);
    };

    let status = coreutils::run(name, tool_argv, &mut ctx);
    std::process::exit(status);
}
