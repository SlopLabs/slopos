//! The tree's checks, in one program.
//!
//! `xtask lint [GATE...]` holds the working tree's sources to the kernel's
//! disciplines, `xtask elf --variant V KERNEL [GATE...]` holds a linked kernel
//! to what its sources promise, `xtask programs PROGRAM...` holds the programs
//! shipped outside the base to `libc.so`, `xtask base-interface` and
//! `xtask libc-exports` hold the two contracts an install depends on, and
//! `xtask clippy` runs clippy over every target the tree builds. A new check
//! is a module here, never a shell script; its fixtures are `#[test]`s, run
//! by `just test-host`.

mod clippy;
mod contract;
mod elf;
#[cfg(test)]
mod fixture;
mod glob;
mod lint;
mod repo;
mod report;
mod rust;
mod toolchain;
mod workspace;

use std::process::ExitCode;

use repo::Repo;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

const USAGE: &str = "\
usage: xtask lint [GATE...]                       source gates (all when none is named)
       xtask elf --variant VARIANT KERNEL [GATE...]  kernel ELF gates (the per-build set when none is named)
       xtask programs PROGRAM...                    programs shipped outside the base
       xtask base-interface [--record | --print]    the private call surface against BASE_INTERFACE
       xtask libc-exports [--record] LIBC_SO        libc.so's exports and header layouts only grow
       xtask clippy [TARGET...]                     clippy over the host, kernel and userland targets
       xtask list                                   every gate, by command";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<bool> {
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    match command.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        "list" => {
            for gate in lint::GATES {
                println!("lint      {}", gate.name);
            }
            for gate in elf::GATES {
                let set = if gate.per_build { "per-build" } else { "named" };
                println!("elf       {} ({set})", gate.name);
            }
            println!("programs  loose-programs");
            println!("contract  base-interface");
            println!("contract  libc-exports");
            Ok(true)
        }
        "lint" => lint::main(&Repo::discover()?, rest),
        "elf" => elf::main(&Repo::discover()?, rest),
        "programs" => elf::loose_programs::main(rest),
        "base-interface" => contract::base_interface::main(&Repo::discover()?, rest),
        "libc-exports" => contract::libc_exports::main(&Repo::discover()?, rest),
        "clippy" => clippy::main(&Repo::discover()?, rest),
        other => Err(format!("unknown command `{other}`\n{USAGE}").into()),
    }
}
