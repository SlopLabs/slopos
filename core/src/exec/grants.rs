//! Program-identity privilege grants.
//!
//! `task.flags` is the whole of SlopOS's privilege model. The privileged bits
//! come from a fixed table keyed on the program's path, applied by
//! [`spawn_program_with_attrs`](super::spawn_program_with_attrs) *after* the
//! syscall boundary stripped every privileged bit the caller asked for: that
//! boundary is purely subtractive — it can reject or strip, never confer.
//!
//! `SYSTEM` deliberately appears nowhere below; naming `/sbin/init` here would
//! let any task re-spawn it and inherit console administration.
//!
//! A grant holds because no process can replace the program it names: the
//! system's programs are the boot slot's base, read-only and pinned, or files
//! the image sealed.

use slopos_abi::task::{
    TASK_FLAG_COMPOSITOR, TASK_FLAG_CONSOLE_ADMIN, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_INSTALL,
    TASK_FLAG_LAUNCH, TASK_FLAG_MOUNT, TASK_FLAG_NET_ADMIN, TASK_FLAG_POWER, TASK_FLAG_PROC_ADMIN,
    TaskPriority,
};

struct ProgramGrant {
    /// Compared byte-for-byte against the NUL-trimmed request: a non-canonical
    /// spelling fails closed rather than making this a parser.
    path: &'static [u8],
    /// OR-ed into the child's flag word when the spawner holds `Launch`.
    flags: u16,
    /// OR-ed into the child's flag word as far as the spawner holds them
    /// itself: authority the program may be handed but never raised to.
    delegated: u16,
    /// Replaces the caller's requested tier, for a program needing one user
    /// space may not ask for.
    priority: Option<TaskPriority>,
}

const PROGRAM_GRANTS: &[ProgramGrant] = &[
    // Every other GUI client's frames land through the compositor, so its
    // latency is a correctness property. `High` is a tier the syscall boundary
    // refuses from anybody.
    ProgramGrant {
        path: b"/bin/compositor",
        // `Launch` because the dock and the shelf spawn programs, and the
        // network popover spawns `/bin/ip` -- which carries NET_ADMIN, so a
        // bound excluding the compositor breaks the network toggle. This is
        // the fourth launcher, and the grant table's own "revisit past about
        // four entries" threshold is therefore already reached.
        flags: TASK_FLAG_COMPOSITOR | TASK_FLAG_LAUNCH,
        delegated: 0,
        priority: Some(TaskPriority::High),
    },
    // Launches every program the user types. Holds `Launch` and nothing else:
    // a shell that also held Power or NET_ADMIN would put that authority in
    // the process that runs every command.
    ProgramGrant {
        path: b"/bin/shell",
        flags: TASK_FLAG_LAUNCH,
        delegated: 0,
        priority: None,
    },
    // Spawns `/bin/shell` onto its PTY slave.
    ProgramGrant {
        path: b"/bin/terminal",
        flags: TASK_FLAG_LAUNCH,
        delegated: 0,
        priority: None,
    },
    // Runs what the paired host asks for, as a shell runs what the user types:
    // `Launch` alone, so `bootctl` and `halt` get their own grants from it and
    // the daemon holding the network connection holds nothing more. Which
    // host that is comes from the sealed base alone, or any program could
    // name one and borrow this grant.
    ProgramGrant {
        path: b"/bin/remoted",
        flags: TASK_FLAG_LAUNCH,
        delegated: 0,
        priority: None,
    },
    // Draws straight to the framebuffer before a compositor exists — what
    // `roulette_draw`'s `requires(display_exclusive)` gates.
    ProgramGrant {
        path: b"/bin/roulette",
        flags: TASK_FLAG_DISPLAY_EXCLUSIVE,
        delegated: 0,
        priority: None,
    },
    // The one writer of the kernel keyboard layout, a single global table
    // feeding every TTY and the compositor; reading it needs nothing.
    ProgramGrant {
        path: b"/bin/keymap",
        flags: TASK_FLAG_CONSOLE_ADMIN,
        delegated: 0,
        priority: None,
    },
    // Every mutating net syscall is gated on this bit, so the control plane
    // has one grammar.
    ProgramGrant {
        path: b"/bin/ip",
        flags: TASK_FLAG_NET_ADMIN,
        delegated: 0,
        priority: None,
    },
    // May enumerate past the dominance relation `process_list` otherwise
    // applies, but gains no power over what it sees: `kill` re-checks dominance
    // against the caller's own flags.
    ProgramGrant {
        path: b"/bin/sysmon",
        flags: TASK_FLAG_PROC_ADMIN,
        delegated: 0,
        priority: None,
    },
    // The only program that may halt or reboot. Power is deliberately not a
    // shell builtin — Linux gates `reboot(2)` on `CAP_SYS_BOOT` and ships
    // `/sbin/halt` separately, `systemctl poweroff` asks logind, and Redox puts
    // the resource behind a daemon. The shell spawns this and waits.
    ProgramGrant {
        path: b"/bin/halt",
        flags: TASK_FLAG_POWER,
        delegated: 0,
        priority: None,
    },
    // Writes the boot partition's slots beneath every filesystem (`Mount`, the
    // raw-device right) and sets the loader's variables and reboots (`Power`).
    ProgramGrant {
        path: b"/bin/bootctl",
        flags: TASK_FLAG_MOUNT | TASK_FLAG_POWER,
        delegated: 0,
        priority: None,
    },
    // Sets the HWP preference and limits and the placement policy every CPU
    // runs under (`cpu_perf_ctl`, classified `Power`).
    ProgramGrant {
        path: b"/bin/cpufreq",
        flags: TASK_FLAG_POWER,
        delegated: 0,
        priority: None,
    },
    // Partitions and formats a disk beneath every filesystem (`Mount`), sets
    // the loader's default (`Power`), and registers the firmware entry and
    // seals the new root's mount points (`Install`).
    ProgramGrant {
        path: b"/bin/installer",
        flags: TASK_FLAG_MOUNT | TASK_FLAG_POWER | TASK_FLAG_INSTALL,
        delegated: 0,
        priority: None,
    },
    // e2fsprogs writes the volume beneath it for a spawner that may itself,
    // the installer; started from the shell, a script cannot format a disk.
    ProgramGrant {
        path: b"/sbin/mke2fs",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    ProgramGrant {
        path: b"/sbin/e2fsck",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    ProgramGrant {
        path: b"/sbin/resize2fs",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    ProgramGrant {
        path: b"/sbin/tune2fs",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    ProgramGrant {
        path: b"/sbin/debugfs",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    ProgramGrant {
        path: b"/sbin/dumpe2fs",
        flags: 0,
        delegated: TASK_FLAG_MOUNT,
        priority: None,
    },
    // Granted to the seat test rather than widening the seat capability, so
    // the shipped answer to "who may take the screen" stays one program. The
    // path ships only in a tests image, and an absent path grants nothing.
    ProgramGrant {
        path: b"/bin/seat_test",
        flags: TASK_FLAG_COMPOSITOR,
        delegated: 0,
        priority: None,
    },
    // Same bargain as the seat test: no shipped program may graft a
    // filesystem onto the namespace.
    ProgramGrant {
        path: b"/bin/mount_test",
        flags: TASK_FLAG_MOUNT,
        delegated: 0,
        priority: None,
    },
    // Keeps its stage in a UEFI variable across the reboots it drives, reads
    // the boot disk's table, and registers SlopOS's firmware entry as the
    // installer will.
    ProgramGrant {
        path: b"/bin/install_test",
        flags: TASK_FLAG_POWER | TASK_FLAG_MOUNT | TASK_FLAG_INSTALL,
        delegated: 0,
        priority: None,
    },
    // Keeps its stage in a UEFI variable across the boots an install takes,
    // gives a foreign disk the firmware entry another system would have, and
    // starts the installer from a shell, which raises it only under `Launch`.
    ProgramGrant {
        path: b"/bin/installer_test",
        flags: TASK_FLAG_POWER | TASK_FLAG_MOUNT | TASK_FLAG_INSTALL | TASK_FLAG_LAUNCH,
        delegated: 0,
        priority: None,
    },
    // Points the resolver at a nameserver it runs on loopback.
    ProgramGrant {
        path: b"/bin/dns_concurrent_test",
        flags: TASK_FLAG_NET_ADMIN,
        delegated: 0,
        priority: None,
    },
    // A dynamically linked program that runs with `AT_SECURE`, so dl_test can
    // show the loader refusing `LD_LIBRARY_PATH` and `$ORIGIN` to it.
    // `PROC_ADMIN` confers read-only enumeration and no mutating class.
    ProgramGrant {
        path: b"/bin/dl_secure_probe",
        flags: TASK_FLAG_PROC_ADMIN,
        delegated: 0,
        priority: None,
    },
];

/// The flags and tier the kernel adds for `path` when the spawner holds
/// `Launch`; `(0, None)` for any program not named above.
pub fn grant_for(path: &[u8]) -> (u16, Option<TaskPriority>) {
    match PROGRAM_GRANTS.iter().find(|grant| grant.path == path) {
        Some(grant) => (grant.flags, grant.priority),
        None => (0, None),
    }
}

/// The flags `path` keeps of those its spawner holds; `0` for any program not
/// named above.
pub fn delegated_for(path: &[u8]) -> u16 {
    PROGRAM_GRANTS
        .iter()
        .find(|grant| grant.path == path)
        .map_or(0, |grant| grant.delegated)
}

/// Where a dynamically linked program's interpreter lives. Not a grant path,
/// but protected as one: the interpreter runs before the program's first
/// instruction, so substituting it substitutes every program that names it.
const INTERPRETER_DIR: &[u8] = b"/lib";

/// Whether `path` is, or is an ancestor of, a path this table keys a privilege
/// on. `path` must be canonical.
///
/// `mount(2)` asks this: a grant is keyed on a *path*, so a writable ramfs
/// over `/bin` would let a planted `halt` spawn with `TASK_FLAG_POWER`. The
/// inode seal cannot see it — a mount changes the namespace rather than an
/// inode.
pub fn covers_grant_path(path: &[u8]) -> bool {
    PROGRAM_GRANTS
        .iter()
        .map(|grant| grant.path)
        .chain(core::iter::once(INTERPRETER_DIR))
        .any(|grant| {
            if grant == path {
                return true;
            }
            // A prefix only at a component boundary: `/bindings` is not `/bin`.
            grant.len() > path.len()
                && grant.starts_with(path)
                && (path == b"/" || grant[path.len()] == b'/')
        })
}
