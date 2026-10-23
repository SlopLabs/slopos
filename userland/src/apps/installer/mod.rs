//! `/bin/installer` — install SlopOS onto a disk from the medium the live
//! system booted, beside whatever else the disk holds.
//!
//! An install writes the partition table — a fresh one, or new entries beside
//! the disk's own — and has the kernel re-read it, formats the new
//! partitions, fills the root, writes both kernel slots, the base and the
//! loader with its configuration, makes slot a the default, checks the root
//! with `e2fsck -fn`, and last registers the firmware entry. Nothing the plan
//! does not name is written, and nothing on a shared ESP outside
//! `\EFI\SlopOS\`.
//!
//! The engine decides and does all of that and only reports it; the CLI
//! asks, with every answer a flag as well as a question, so a check drives
//! the program a person runs. Started without its capabilities, it is
//! privd's `org.slopos.install`, and its questions are asked through the
//! requester's terminal.

mod boot;
mod cli;
mod disk;
mod engine;
mod medium;
mod root;

use crate::syscall::core as sys_core;

pub fn installer_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = cli::run(&args) {
        eprintln!("installer: {message}");
        sys_core::exit_with_code(1);
    }
}
