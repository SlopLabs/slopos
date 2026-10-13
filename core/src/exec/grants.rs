//! Program-identity grants: the capabilities a spawn adds for the program it
//! loads.
//!
//! A fixed table keyed on the program's path, applied by
//! [`spawn_program_with_attrs`](super::spawn_program_with_attrs) and narrowed
//! to by `execve`. Init is deliberately absent: naming `/sbin/init` here would
//! let any task re-spawn it and inherit what init holds. So is the
//! compositor, whose seats and tier only init delegates.
//!
//! A grant holds because no process can replace the program it names: the
//! system's programs are the boot slot's base, read-only and pinned, or files
//! the image sealed.

use slopos_abi::caps::{
    BOOT_ENTRY, CONSOLE_CONFIG, DAC_OVERRIDE, DISPLAY_SEAT, LAUNCH, MOUNT, NET_ADMIN, POWER,
    RAW_BLOCK, SEAL, SYS_INSPECT,
};
use slopos_ostd::authority::CAP_NONE;

/// What the installer does: partitions and formats a disk beneath every
/// filesystem, mounts the new root, lays out its ownership, sets the loader's
/// default, registers the firmware entry and seals the new root's mount
/// points.
const INSTALL: u64 = RAW_BLOCK | MOUNT | POWER | BOOT_ENTRY | SEAL | DAC_OVERRIDE;

struct ProgramGrant {
    /// Compared byte-for-byte against the NUL-trimmed request: a non-canonical
    /// spelling fails closed rather than making this a parser.
    path: &'static [u8],
    /// Added to the child's mask when the spawner holds `Launch`.
    caps: u64,
    /// Added to the child's mask as far as the spawner holds them itself:
    /// authority the program may be handed but never raised to.
    delegated: u64,
}

const PROGRAM_GRANTS: &[ProgramGrant] = &[
    // Launches every program the user types. Holds `Launch` and nothing else:
    // a shell that also held `Power` or `NetAdmin` would put that authority in
    // the process that runs every command.
    ProgramGrant {
        path: b"/bin/shell",
        caps: LAUNCH,
        delegated: CAP_NONE,
    },
    // Spawns `/bin/shell` onto its PTY slave.
    ProgramGrant {
        path: b"/bin/terminal",
        caps: LAUNCH,
        delegated: CAP_NONE,
    },
    // Runs what the paired host asks for, as a shell runs what the user types:
    // `Launch` alone, so `bootctl` and `halt` get their own grants from it and
    // the daemon holding the network connection holds nothing more. Which
    // host that is comes from the sealed base alone, or any program could
    // name one and borrow this grant.
    ProgramGrant {
        path: b"/bin/remoted",
        caps: LAUNCH,
        delegated: CAP_NONE,
    },
    // Draws straight to the framebuffer before a compositor exists.
    ProgramGrant {
        path: b"/bin/roulette",
        caps: DISPLAY_SEAT,
        delegated: CAP_NONE,
    },
    // The one writer of the kernel keyboard layout, a single global table
    // feeding every TTY and the compositor, and of `/etc/keymap`; reading it
    // needs nothing.
    ProgramGrant {
        path: b"/bin/keymap",
        caps: CONSOLE_CONFIG | DAC_OVERRIDE,
        delegated: CAP_NONE,
    },
    // Every mutating net syscall is gated on `NetAdmin`, so the control plane
    // has one grammar.
    ProgramGrant {
        path: b"/bin/ip",
        caps: NET_ADMIN,
        delegated: CAP_NONE,
    },
    // The diagnostic console's informational commands print every task and
    // every CPU. Commands that change the machine stay the physical console's.
    ProgramGrant {
        path: b"/bin/kconsole",
        caps: SYS_INSPECT,
        delegated: CAP_NONE,
    },
    // Starts, stops and reports the sampling profiler (`prof_ctl`).
    ProgramGrant {
        path: b"/bin/prof",
        caps: SYS_INSPECT,
        delegated: CAP_NONE,
    },
    // The only program that may halt or reboot. Power is deliberately not a
    // shell builtin — Linux gates `reboot(2)` on `CAP_SYS_BOOT` and ships
    // `/sbin/halt` separately, `systemctl poweroff` asks logind, and Redox puts
    // the resource behind a daemon. The shell spawns this and waits.
    ProgramGrant {
        path: b"/bin/halt",
        caps: POWER,
        delegated: CAP_NONE,
    },
    // Writes the boot partition's slots and reads the crash records beneath
    // every filesystem, sets the loader's variables and reboots, and forgets
    // a slot's last boot under `/var/lib/slopos/slots`.
    ProgramGrant {
        path: b"/bin/bootctl",
        caps: RAW_BLOCK | POWER | DAC_OVERRIDE,
        delegated: CAP_NONE,
    },
    // Sets the HWP preference and limits and the placement policy every CPU
    // runs under (`cpu_perf_ctl`, classified `Power`).
    ProgramGrant {
        path: b"/bin/cpufreq",
        caps: POWER,
        delegated: CAP_NONE,
    },
    ProgramGrant {
        path: b"/bin/installer",
        caps: INSTALL,
        delegated: CAP_NONE,
    },
    // e2fsprogs writes the volume beneath it for a spawner that may itself,
    // the installer; started from the shell, a script cannot format a disk.
    ProgramGrant {
        path: b"/sbin/mke2fs",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
    ProgramGrant {
        path: b"/sbin/e2fsck",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
    ProgramGrant {
        path: b"/sbin/resize2fs",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
    ProgramGrant {
        path: b"/sbin/tune2fs",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
    ProgramGrant {
        path: b"/sbin/debugfs",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
    ProgramGrant {
        path: b"/sbin/dumpe2fs",
        caps: CAP_NONE,
        delegated: RAW_BLOCK,
    },
];

/// The capabilities the kernel adds for `path` when the spawner holds
/// `Launch`; `CAP_NONE` for any program not named above.
pub fn grant_for(path: &[u8]) -> u64 {
    PROGRAM_GRANTS
        .iter()
        .find(|grant| grant.path == path)
        .map_or(CAP_NONE, |grant| grant.caps)
}

/// The capabilities `path` keeps of those its spawner holds; `CAP_NONE` for
/// any program not named above.
pub fn delegated_for(path: &[u8]) -> u64 {
    PROGRAM_GRANTS
        .iter()
        .find(|grant| grant.path == path)
        .map_or(CAP_NONE, |grant| grant.delegated)
}

/// Every path the table grants something to.
#[cfg(feature = "test-hooks")]
pub fn granted_paths() -> impl Iterator<Item = &'static [u8]> {
    PROGRAM_GRANTS.iter().map(|grant| grant.path)
}

/// Where a dynamically linked program's interpreter lives. Not a grant path,
/// but protected as one: the interpreter runs before the program's first
/// instruction, so substituting it substitutes every program that names it.
const INTERPRETER_DIR: &[u8] = b"/lib";

/// Whether `path` is, or is an ancestor of, a path this table keys a privilege
/// on. `path` must be canonical.
///
/// `mount(2)` asks this: a grant is keyed on a *path*, so a writable ramfs
/// over `/bin` would let a planted `halt` spawn holding `Power`. The inode
/// seal cannot see it — a mount changes the namespace rather than an inode.
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
