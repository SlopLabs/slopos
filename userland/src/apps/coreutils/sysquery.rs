//! What the machine says about itself: `nproc`, `uname`, `whoami`, `pwd`,
//! `date`, `ps`.

use slopos_abi::task::TaskStatus;

use crate::syscall::core as sys_core;
use crate::syscall::{UserTaskEntry, UserUtsname};

use super::io::Sink;
use super::opts::{Opt, Opts};
use super::time::{self, MONTHS, WEEKDAYS};
use super::{Ctx, Tool};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "nproc",
        desc: "Print the number of processing units",
        usage: "nproc",
        run: nproc,
    },
    Tool {
        name: "uname",
        desc: "Print system information",
        usage: UNAME_USAGE,
        run: uname,
    },
    Tool {
        name: "whoami",
        desc: "Print the effective user name",
        usage: "whoami",
        run: whoami,
    },
    Tool {
        name: "pwd",
        desc: "Print the working directory",
        usage: "pwd",
        run: pwd,
    },
    Tool {
        name: "date",
        desc: "Print the date and time",
        usage: DATE_USAGE,
        run: date,
    },
    Tool {
        name: "ps",
        desc: "List tasks",
        usage: PS_USAGE,
        run: ps,
    },
];

const UNAME_USAGE: &str = "uname [-asrmnv]";
const DATE_USAGE: &str = "date [-u] [+format]";
const PS_USAGE: &str = "ps [-ef]";

fn nproc(ctx: &mut Ctx, _argv: &[&[u8]]) -> i32 {
    ctx.out.u(u64::from(sys_core::get_cpu_count()));
    ctx.out.nl();
    0
}

/// SlopOS is single-user at uid 0 — a decided property of the system, not a
/// placeholder for an account database that is coming later.
fn whoami(ctx: &mut Ctx, _argv: &[&[u8]]) -> i32 {
    ctx.out.s("root");
    ctx.out.nl();
    0
}

fn pwd(ctx: &mut Ctx, _argv: &[&[u8]]) -> i32 {
    match std::env::current_dir() {
        Ok(dir) => match dir.to_str() {
            Some(text) => {
                ctx.out.s(text);
                ctx.out.nl();
                0
            }
            None => {
                ctx.warn(b"the working directory is not valid UTF-8");
                1
            }
        },
        Err(error) => {
            ctx.warn(super::io::io_message(&error).as_bytes());
            1
        }
    }
}

fn utsname_field(field: &[u8]) -> &str {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).unwrap_or("")
}

fn uname(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let (mut sysname, mut nodename, mut release, mut version, mut machine) =
        (false, false, false, false, false);
    let mut opts = Opts::new(argv, "asrmnv");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'a') => {
                sysname = true;
                nodename = true;
                release = true;
                version = true;
                machine = true;
            }
            Opt::Flag(b's') => sysname = true,
            Opt::Flag(b'n') => nodename = true,
            Opt::Flag(b'r') => release = true,
            Opt::Flag(b'v') => version = true,
            Opt::Flag(b'm') => machine = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(UNAME_USAGE);
            }
            _ => {}
        }
    }
    if !opts.operands().is_empty() {
        return ctx.usage(UNAME_USAGE);
    }
    if !(sysname || nodename || release || version || machine) {
        sysname = true;
    }

    let mut uts = UserUtsname::new();
    if sys_core::uname(&mut uts) < 0 {
        ctx.warn(b"cannot read the system name");
        return 1;
    }

    let selected = [
        (sysname, &uts.sysname[..]),
        (nodename, &uts.nodename[..]),
        (release, &uts.release[..]),
        (version, &uts.version[..]),
        (machine, &uts.machine[..]),
    ];
    let mut first = true;
    for (wanted, field) in selected {
        if !wanted {
            continue;
        }
        if !first {
            ctx.out.b(b' ');
        }
        ctx.out.s(utsname_field(field));
        first = false;
    }
    ctx.out.nl();
    0
}

fn two_digits(out: &mut Sink, value: u64, pad: u8) {
    if value < 10 {
        out.b(pad);
    }
    out.u(value);
}

fn date(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut opts = Opts::new(argv, "u");
    for opt in opts.by_ref() {
        match opt {
            // SlopOS keeps no timezone database; the clock is UTC already.
            Opt::Flag(b'u') => {}
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(DATE_USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    let format: &[u8] = match operands.first() {
        Some(operand) if operand.first() == Some(&b'+') => &operand[1..],
        Some(_) => {
            ctx.warn(b"setting the clock is not supported");
            return ctx.usage(DATE_USAGE);
        }
        None => b"%a %b %e %H:%M:%S UTC %Y",
    };
    if operands.len() > 1 {
        return ctx.usage(DATE_USAGE);
    }

    // `realtime_secs` answers `None` until the kernel has a wall-clock anchor;
    // reporting 1970 instead would be a plausible-looking lie.
    let Some(epoch) = sys_core::realtime_secs() else {
        ctx.warn(b"the system clock has not been set");
        return 1;
    };

    let time::Utc {
        year,
        month,
        day,
        hour,
        minute,
        second,
        weekday,
        yday,
    } = time::utc_from_epoch(epoch);

    let mut i = 0;
    while i < format.len() {
        if format[i] != b'%' {
            let start = i;
            while i < format.len() && format[i] != b'%' {
                i += 1;
            }
            ctx.out.write(&format[start..i]);
            continue;
        }
        i += 1;
        let Some(&spec) = format.get(i) else {
            ctx.out.b(b'%');
            break;
        };
        i += 1;
        match spec {
            b'Y' => ctx.out.i(year),
            b'm' => two_digits(&mut ctx.out, u64::from(month), b'0'),
            b'd' => two_digits(&mut ctx.out, u64::from(day), b'0'),
            b'e' => two_digits(&mut ctx.out, u64::from(day), b' '),
            b'H' => two_digits(&mut ctx.out, u64::from(hour), b'0'),
            b'M' => two_digits(&mut ctx.out, u64::from(minute), b'0'),
            b'S' => two_digits(&mut ctx.out, u64::from(second), b'0'),
            b'j' => {
                if yday < 100 {
                    ctx.out.b(b'0');
                }
                two_digits(&mut ctx.out, u64::from(yday), b'0');
            }
            b'b' => ctx.out.s(MONTHS[month as usize - 1]),
            b'a' => ctx.out.s(WEEKDAYS[weekday as usize]),
            b'F' => {
                ctx.out.i(year);
                ctx.out.b(b'-');
                two_digits(&mut ctx.out, u64::from(month), b'0');
                ctx.out.b(b'-');
                two_digits(&mut ctx.out, u64::from(day), b'0');
            }
            b'T' => {
                two_digits(&mut ctx.out, u64::from(hour), b'0');
                ctx.out.b(b':');
                two_digits(&mut ctx.out, u64::from(minute), b'0');
                ctx.out.b(b':');
                two_digits(&mut ctx.out, u64::from(second), b'0');
            }
            b's' => ctx.out.i(epoch),
            b'%' => ctx.out.b(b'%'),
            other => {
                ctx.out.b(b'%');
                ctx.out.b(other);
            }
        }
    }
    ctx.out.nl();
    0
}

/// Cap on the tasks one listing reports; `process_list` fills what fits.
const MAX_TASKS: usize = 256;

fn state_label(state: u8) -> &'static str {
    match TaskStatus::from_u8(state) {
        TaskStatus::Running => "Run",
        TaskStatus::Ready => "Ready",
        TaskStatus::Blocked => "Block",
        TaskStatus::Stopped => "Stop",
        TaskStatus::Zombie => "Zombie",
        TaskStatus::Terminated => "Dead",
        TaskStatus::Invalid => "--",
    }
}

fn column(out: &mut Sink, text: &str, width: usize) {
    for _ in text.len()..width {
        out.b(b' ');
    }
    out.s(text);
}

fn ps(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut full = false;
    let mut opts = Opts::new(argv, "ef");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'e') => {}
            Opt::Flag(b'f') => full = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(PS_USAGE);
            }
            _ => {}
        }
    }
    if !opts.operands().is_empty() {
        return ctx.usage(PS_USAGE);
    }

    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = sys_core::process_list(&mut tasks);
    if count < 0 {
        ctx.warn(b"cannot read the task list");
        return 1;
    }
    let count = (count as usize).min(tasks.len());

    column(&mut ctx.out, "PID", 6);
    if full {
        column(&mut ctx.out, "PPID", 6);
    }
    column(&mut ctx.out, "STATE", 7);
    ctx.out.s(" COMMAND");
    ctx.out.nl();

    for task in &tasks[..count] {
        ctx.out.u_right(u64::from(task.task_id), 6);
        if full {
            ctx.out.u_right(u64::from(task.parent_task_id), 6);
        }
        column(&mut ctx.out, state_label(task.state), 7);
        ctx.out.b(b' ');
        let end = task
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(task.name.len());
        ctx.out.write(&task.name[..end]);
        ctx.out.nl();
    }
    0
}
