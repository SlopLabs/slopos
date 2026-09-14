//! The utilities, as executables.
//!
//! `/bin/coreutils` is a multicall binary and `/bin/<tool>` a symlink to it,
//! so `argv[0]` selects the tool. The shell's builtins dispatch into the same
//! functions, so there is no second `ls`.
//!
//! A tool takes POSIX-style byte argv and a [`Ctx`] saying where its output
//! goes. It never names fd 1, never flushes and never exits.

pub mod fsutil;
pub mod input;
pub mod io;
pub mod opts;
pub mod pattern;
pub mod regex;
pub mod time;

mod archive;
mod deflate;
mod diffs;
mod files;
mod find;
mod grep;
mod gzip;
mod hash;
mod listing;
mod pager;
mod pathname;
mod sed;
mod shellprim;
mod sortuniq;
mod stty;
mod sysquery;
mod textio;
mod xargs;

pub use io::Ctx;

pub struct Tool {
    pub name: &'static str,
    /// One line for `help`, the shell's completion and `coreutils --list`.
    pub desc: &'static str,
    pub usage: &'static str,
    pub run: fn(&mut Ctx, &[&[u8]]) -> i32,
}

static TOOL_SETS: &[&[Tool]] = &[
    archive::TOOLS,
    diffs::TOOLS,
    files::TOOLS,
    find::TOOLS,
    grep::TOOLS,
    gzip::TOOLS,
    hash::TOOLS,
    listing::TOOLS,
    pager::TOOLS,
    pathname::TOOLS,
    sed::TOOLS,
    shellprim::TOOLS,
    sortuniq::TOOLS,
    stty::TOOLS,
    sysquery::TOOLS,
    textio::TOOLS,
    xargs::TOOLS,
];

pub fn find_tool(name: &[u8]) -> Option<&'static Tool> {
    TOOL_SETS
        .iter()
        .flat_map(|set| set.iter())
        .find(|tool| tool.name.as_bytes() == name)
}

pub fn tools() -> impl Iterator<Item = &'static Tool> {
    TOOL_SETS.iter().flat_map(|set| set.iter())
}

/// Status POSIX reserves for "the utility could not be found".
pub const STATUS_NOT_FOUND: i32 = 127;

/// Run `name` with `argv` (whose first element is the name as the caller spelt
/// it). Flushes `ctx.out` before returning, so a tool's last line is on the
/// wire before its status is read.
pub fn run(name: &[u8], argv: &[&[u8]], ctx: &mut Ctx) -> i32 {
    let Some(tool) = find_tool(name) else {
        ctx.set_tool("coreutils");
        ctx.warn_at(name, b"not found");
        return STATUS_NOT_FOUND;
    };
    ctx.set_tool(tool.name);
    let status = (tool.run)(ctx, argv);
    ctx.out.flush();
    ctx.err.flush();
    status
}

/// `argv[0]`'s final component — the name the caller invoked, which for a
/// `/bin/<tool>` symlink is the tool.
pub fn invoked_name<'a>(argv: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    let first = argv.first()?;
    Some(match first.iter().rposition(|&b| b == b'/') {
        Some(i) => &first[i + 1..],
        None => &first[..],
    })
}

/// The name a multicall invocation selects, with a leading `coreutils` meaning
/// "the tool is `argv[1]`". Its own options are only its own when it was
/// invoked under its own name, or `/bin/ls -l` would never reach `ls`.
pub fn select<'a>(argv: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    let name = invoked_name(argv)?;
    if name == b"coreutils" {
        let rest = argv.get(1..)?;
        let tool = rest.first()?;
        return Some((tool, rest));
    }
    Some((name, argv))
}
