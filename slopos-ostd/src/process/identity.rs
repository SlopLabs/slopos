//! What the kernel attests about a process to a peer: who it acts as and which
//! program it runs, captured when a socket connects. The ids stand as
//! captured; the program stands only while the process it was taken from
//! still runs it.

use core::num::NonZeroU64;

use super::{process_for_handle, unpack_process_handle};

/// Which program a process runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Program {
    /// The base file with this inode, which names the generation it is of:
    /// the image, every script of its `#!` chain and its ELF interpreter are
    /// the base's, and no code from outside the base has been mapped
    /// executable since.
    Base(NonZeroU64),
    /// The file with this inode of the extension activation it names: the
    /// image is that activation's, every other link of its chain is the
    /// base's or that activation's, and no code from elsewhere has been
    /// mapped executable since.
    Extension(NonZeroU64),
    Other,
}

/// The bit an extension's program carries in its word: activation numbers,
/// an inode's high half, stay below 2^31.
const EXTENSION_BIT: u64 = 1 << 63;

impl Program {
    pub(super) const fn to_raw(self) -> u64 {
        match self {
            Self::Base(inode) => inode.get() & !EXTENSION_BIT,
            Self::Extension(inode) => inode.get() | EXTENSION_BIT,
            Self::Other => 0,
        }
    }

    pub(super) const fn from_raw(raw: u64) -> Self {
        match NonZeroU64::new(raw & !EXTENSION_BIT) {
            Some(inode) if raw & EXTENSION_BIT != 0 => Self::Extension(inode),
            Some(inode) => Self::Base(inode),
            None => Self::Other,
        }
    }

    /// Whether code of `file`, a file the base or an activation serves, may
    /// run under this program: a base file under any attested program, an
    /// activation's under a program of that activation alone.
    pub fn admits(self, file: Self) -> bool {
        match (self, file) {
            (Self::Other, _) | (_, Self::Other) => false,
            (_, Self::Base(_)) => true,
            (Self::Extension(mine), Self::Extension(theirs)) => {
                mine.get() >> 32 == theirs.get() >> 32
            }
            (Self::Base(_), Self::Extension(_)) => false,
        }
    }
}

/// A process as a peer sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// What `getpid` answers it. Task ids are never reused, so this names the
    /// process for good, and its exit is observable after the fact.
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub program: Program,
    /// The process, as a packed generation-checked handle, and the program
    /// instance it ran when this was captured.
    pub process: u64,
    pub instance: u64,
}

impl Identity {
    /// The kernel acting for no process: no pid, so no pidfd, and no ids, as
    /// `SO_PEERCRED` reads with no peer.
    pub const KERNEL: Self = Self {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
        program: Program::Other,
        process: 0,
        instance: 0,
    };

    /// This identity as it stands now: its program only while the process it
    /// was taken from is live and runs the program instance it ran then.
    pub fn now(self) -> Self {
        let runs = unpack_process_handle(self.process)
            .and_then(process_for_handle)
            .is_some_and(|process| {
                !process.has_exited() && process.program_instance() == self.instance
            });
        if runs {
            self
        } else {
            Self {
                program: Program::Other,
                ..self
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_round_trips_through_its_word_and_zero_is_other() {
        assert_eq!(Program::from_raw(0), Program::Other);
        assert_eq!(Program::from_raw(EXTENSION_BIT), Program::Other);
        assert_eq!(Program::Other.to_raw(), 0);
        for raw in [1u64, 2, (0x7fff_ffff << 32) | 7] {
            assert_eq!(Program::from_raw(raw).to_raw(), raw);
            assert!(matches!(Program::from_raw(raw), Program::Base(_)));
            let extension = raw | EXTENSION_BIT;
            assert_eq!(Program::from_raw(extension).to_raw(), extension);
            assert!(matches!(
                Program::from_raw(extension),
                Program::Extension(i) if i.get() == raw
            ));
        }
    }

    #[test]
    fn a_program_admits_the_base_and_its_own_activation_only() {
        let inode = |activation: u64, at: u64| NonZeroU64::new((activation << 32) | at).unwrap();
        let base = Program::Base(inode(1, 1));
        let ext = Program::Extension(inode(5, 1));
        assert!(base.admits(Program::Base(inode(2, 9))));
        assert!(!base.admits(Program::Extension(inode(5, 2))));
        assert!(ext.admits(Program::Base(inode(1, 3))));
        assert!(ext.admits(Program::Extension(inode(5, 2))));
        assert!(!ext.admits(Program::Extension(inode(6, 1))));
        assert!(!Program::Other.admits(Program::Base(inode(1, 1))));
        assert!(!base.admits(Program::Other));
    }
}
