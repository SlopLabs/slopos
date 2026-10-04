//! `/bin/cpufreq` — what the CPUs run at, the settings they run under, and
//! what a hybrid part's cores are worth to a thread.
//!
//! Reads `cpu_perf`, which every user may; `set` calls `cpu_perf_ctl`, which
//! needs the `Power` the kernel grants this program by path.

mod bench;
mod run;
pub mod sample;
mod set;
mod status;

use std::io::Write;

use crate::syscall::core as sys_core;

const USAGE: &str = "usage: cpufreq [status]
       cpufreq watch [-i MS] [-n COUNT]
       cpufreq run -- CMD [ARGS...]
       cpufreq set hwp | epp <name|0-255> | limits <min> <max> | placement flat|ranked
       cpufreq bench [-t MS] [-n RUNS]";

/// Exit status of a usage error, as POSIX utilities give it.
const STATUS_USAGE: i32 = 2;

fn usage() -> i32 {
    eprintln!("{USAGE}");
    STATUS_USAGE
}

/// `-x VALUE` / `-xVALUE` options, each a positive number, from `allowed`.
fn numeric_opts(args: &[String], allowed: &[char]) -> Result<Vec<(char, u64)>, String> {
    let mut opts = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut chars = arg.chars();
        let flag = match (chars.next(), chars.next()) {
            (Some('-'), Some(flag)) if allowed.contains(&flag) => flag,
            _ => return Err(format!("unexpected argument {arg:?}")),
        };
        let inline = &arg[2..];
        let text = if inline.is_empty() {
            rest.next().ok_or(format!("-{flag} takes a number"))?
        } else {
            inline
        };
        match text.parse::<u64>() {
            Ok(value) if value > 0 => opts.push((flag, value)),
            _ => return Err(format!("-{flag} takes a positive number, not {text:?}")),
        }
    }
    Ok(opts)
}

fn dispatch(args: &[String]) -> Result<i32, String> {
    let Some(command) = args.first() else {
        return status::status();
    };
    let rest = &args[1..];
    match command.as_str() {
        "status" if rest.is_empty() => status::status(),
        "watch" => {
            let mut interval_ms = 1000;
            let mut count = None;
            for (flag, value) in numeric_opts(rest, &['i', 'n'])? {
                match flag {
                    'i' => interval_ms = value,
                    _ => count = Some(value),
                }
            }
            status::watch(interval_ms, count)
        }
        "run" => match rest.strip_prefix(&["--".to_owned()]).unwrap_or(rest) {
            [] => Ok(usage()),
            command => run::run(command),
        },
        "set" => set::set(rest),
        "bench" => {
            let mut options = bench::Options::default();
            for (flag, value) in numeric_opts(rest, &['t', 'n'])? {
                match flag {
                    't' => options.run_ms = value,
                    _ => options.placed_runs = value as usize,
                }
            }
            bench::bench(options)
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(0)
        }
        _ => Ok(usage()),
    }
}

pub fn cpufreq_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let status = dispatch(&args).unwrap_or_else(|message| {
        eprintln!("cpufreq: {message}");
        1
    });
    let _ = std::io::stdout().flush();
    sys_core::exit_with_code(status)
}
