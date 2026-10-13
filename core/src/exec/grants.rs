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
//! the image sealed. The mechanisms that held authority here are privd's now;
//! what remains are the launchers and roulette.

use slopos_abi::caps::{DISPLAY_SEAT, LAUNCH};
use slopos_ostd::authority::CAP_NONE;

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
