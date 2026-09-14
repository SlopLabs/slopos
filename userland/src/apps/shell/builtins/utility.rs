//! The shell's door into `apps::coreutils`.
//!
//! `echo`, `printf`, `test`/`[`, `true` and `false` resolve without a fork, as
//! every shell resolves them. The implementation is `/bin/<name>`'s; this only
//! aims its output at whatever the builtin's `>` redirected to.

use crate::apps::coreutils;

use super::super::display::shell_output_target;

fn run(name: &'static [u8], argv: &[&[u8]]) -> i32 {
    let (fd, tty) = shell_output_target();
    let mut ctx = coreutils::Ctx::with_out(fd, tty);
    coreutils::run(name, argv, &mut ctx)
}

pub fn cmd_echo(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"echo", argv)
}

pub fn cmd_printf(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"printf", argv)
}

pub fn cmd_test(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"test", argv)
}

/// `[` is `test` with a required `]`, and the tool of that name enforces it.
pub fn cmd_bracket(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"[", argv)
}

pub fn cmd_true(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"true", argv)
}

pub fn cmd_false(_argc: i32, argv: &[&[u8]]) -> i32 {
    run(b"false", argv)
}
