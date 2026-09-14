//! SlopOS-specific utility builtins: random, roulette, wl.
//!
//! `sleep`, `seq` and `yes` left for `apps::coreutils` — a utility a spawned
//! build tool cannot reach is not a utility.

use crate::syscall::{UserSysInfo, core as sys_core, roulette};
use std::thread;
use std::time::Duration;

use super::super::NL;
use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::jobs::{parse_u32_arg, write_u64};

pub fn cmd_random(argc: i32, argv: &[&[u8]]) -> i32 {
    let raw = sys_core::random_next();
    let value = if argc >= 2 {
        let Some(max) = parse_u32_arg(argv[1]) else {
            shell_write_idx(b"random: invalid max\n", COLOR_ERROR_RED);
            return 1;
        };
        if max == 0 {
            shell_write_idx(b"random: max must be > 0\n", COLOR_ERROR_RED);
            return 1;
        }
        raw % max
    } else {
        raw
    };
    write_u64(value as u64);
    shell_write(NL.as_bytes());
    0
}

pub fn cmd_roulette(_argc: i32, _argv: &[&[u8]]) -> i32 {
    shell_write(b"=== WHEEL OF FATE ===\n");
    shell_write(b"Spinning...\n");

    let spin = roulette::spin();
    let fate = spin as u32;

    thread::sleep(Duration::from_millis(200));

    shell_write(b"Fate number: ");
    write_u64(fate as u64);
    shell_write(NL.as_bytes());

    let is_win = (fate & 1) == 1;

    if is_win {
        shell_write(b"The Wheel smiles upon you. W +10\n");
    } else {
        shell_write(b"The Wheel demands its toll. Rebooting...\n");
    }

    // On loss the kernel reboots -- this call may not return.
    roulette::result(spin);

    0
}

pub fn cmd_wl(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"wl: failed to query balance\n", COLOR_ERROR_RED);
        return 1;
    }

    let balance = info.wl_balance;

    shell_write(format!("W/L Balance: {balance}\n").as_bytes());

    if balance > 100 {
        shell_write(b"The Wheel favors the bold.\n");
    } else if balance > 0 {
        shell_write(b"Fate is cautiously on your side.\n");
    } else if balance == 0 {
        shell_write(b"Perfectly balanced, as all slop should be.\n");
    } else if balance > -100 {
        shell_write(b"The house is winning. Spin again?\n");
    } else {
        shell_write(b"Deep in the red. The Wheel remembers.\n");
    }
    0
}
