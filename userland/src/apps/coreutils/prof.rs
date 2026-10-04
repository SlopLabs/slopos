//! `prof`: start, stop and report the kernel's sampling profiler.
//!
//! A report is printed into the kernel log; `prof report` copies the lines it
//! added there to its output, so `prof report x > x.log` is a file
//! `scripts/prof_report.py` reads.

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    PROF_LABEL_MAX, PROF_OP_REPORT, PROF_OP_START, PROF_OP_STATUS, PROF_OP_STOP,
};

use crate::kmsg;
use crate::syscall::core as sys_core;

use super::{Ctx, Tool};

pub static TOOLS: &[Tool] = &[Tool {
    name: "prof",
    desc: "Start, stop or report the kernel's sampling profiler",
    usage: USAGE,
    run: prof,
}];

const USAGE: &str = "prof start | stop | status | report [LABEL]";
const DEFAULT_LABEL: &[u8] = b"runtime";

fn label_ok(label: &[u8]) -> bool {
    (1..=PROF_LABEL_MAX).contains(&label.len())
        && label
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn control(ctx: &mut Ctx, op: u64, label: &[u8]) -> Option<i64> {
    let rc = sys_core::prof_ctl(op, label);
    if rc < 0 {
        match Errno::from_raw(rc as i32) {
            Some(errno) => ctx.warn(errno.description().as_bytes()),
            None => ctx.warn(format!("error {rc}").as_bytes()),
        }
        return None;
    }
    Some(rc)
}

fn report(ctx: &mut Ctx, label: &[u8]) -> i32 {
    let before = match kmsg::read() {
        Ok(bytes) => bytes,
        Err(error) => {
            ctx.warn_io(kmsg::PATH.as_bytes(), &error);
            return 1;
        }
    };
    if control(ctx, PROF_OP_REPORT, label).is_none() {
        return 1;
    }
    let after = match kmsg::read() {
        Ok(bytes) => bytes,
        Err(error) => {
            ctx.warn_io(kmsg::PATH.as_bytes(), &error);
            return 1;
        }
    };
    let new = kmsg::added(&before, &after).unwrap_or_else(|| {
        ctx.warn(b"the kernel log wrapped past the report's start; its first lines are lost");
        &after
    });
    let mut prefix = b"PROF[".to_vec();
    prefix.extend_from_slice(label);
    prefix.extend_from_slice(b"]: ");
    let mut lines = 0usize;
    for line in new.split(|&b| b == b'\n') {
        if line.starts_with(&prefix) {
            ctx.out.write(line);
            ctx.out.nl();
            lines += 1;
        }
    }
    if lines == 0 {
        ctx.warn(b"the report added no lines to the kernel log");
        return 1;
    }
    0
}

fn prof(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    match argv.get(1..).unwrap_or_default() {
        [b"start"] => control(ctx, PROF_OP_START, b"").map_or(1, |_| 0),
        [b"stop"] => control(ctx, PROF_OP_STOP, b"").map_or(1, |_| 0),
        [b"status"] => match control(ctx, PROF_OP_STATUS, b"") {
            Some(state) => {
                ctx.out.s(if state != 0 { "running" } else { "stopped" });
                ctx.out.nl();
                0
            }
            None => 1,
        },
        [b"report"] => report(ctx, DEFAULT_LABEL),
        [b"report", label] if label_ok(label) => report(ctx, label),
        [b"report", label] => {
            let why = format!(
                "a label is 1 to {PROF_LABEL_MAX} bytes of letters, digits, '_', '.' and '-'"
            );
            ctx.warn_at(label, why.as_bytes());
            2
        }
        _ => ctx.usage(USAGE),
    }
}
