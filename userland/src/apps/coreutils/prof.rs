//! `prof`: start, stop and report the kernel's sampling profiler.
//!
//! A report is printed into the kernel log; `prof report` copies the lines it
//! added there to its output, so `prof report x > x.log` is a file
//! `scripts/prof_report.py` reads.

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    PROF_LABEL_MAX, PROF_OP_REPORT, PROF_OP_START, PROF_OP_STATUS, PROF_OP_STOP,
};

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
const KMSG: &str = "/dev/kmsg";
/// How much of the log's tail locates the old end once the ring has wrapped.
const ANCHOR_BYTES: usize = 4096;

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

/// What the log gained between `before` and `after`. The log is a ring read
/// by offset, so once it is full its old end has moved down by however much
/// was added; the tail of `before` finds it again.
fn added<'a>(before: &[u8], after: &'a [u8]) -> Option<&'a [u8]> {
    if after.len() > before.len() && after[..before.len()] == *before {
        return Some(&after[before.len()..]);
    }
    let anchor = &before[before.len().saturating_sub(ANCHOR_BYTES)..];
    if anchor.is_empty() {
        return Some(after);
    }
    let at = after
        .windows(anchor.len())
        .rposition(|window| window == anchor)?;
    Some(&after[at + anchor.len()..])
}

fn report(ctx: &mut Ctx, label: &[u8]) -> i32 {
    let before = match std::fs::read(KMSG) {
        Ok(bytes) => bytes,
        Err(error) => {
            ctx.warn_io(KMSG.as_bytes(), &error);
            return 1;
        }
    };
    if control(ctx, PROF_OP_REPORT, label).is_none() {
        return 1;
    }
    let after = match std::fs::read(KMSG) {
        Ok(bytes) => bytes,
        Err(error) => {
            ctx.warn_io(KMSG.as_bytes(), &error);
            return 1;
        }
    };
    let new = added(&before, &after).unwrap_or_else(|| {
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
