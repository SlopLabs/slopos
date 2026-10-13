//! What the kernel attests about a process to a peer: who it acts as and which
//! program it runs, captured when a socket connects, so nothing the process
//! does afterwards changes what the peer was told.

use core::num::NonZeroU64;

/// Which program a process runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Program {
    /// The base file with this inode in the boot slot's base: the image, every
    /// script of its `#!` chain and its ELF interpreter are the base's, and no
    /// code from outside the base has been mapped executable since.
    Base(NonZeroU64),
    Other,
}

impl Program {
    pub(super) const fn to_raw(self) -> u64 {
        match self {
            Self::Base(inode) => inode.get(),
            Self::Other => 0,
        }
    }

    pub(super) const fn from_raw(raw: u64) -> Self {
        match NonZeroU64::new(raw) {
            Some(inode) => Self::Base(inode),
            None => Self::Other,
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
}

impl Identity {
    /// The kernel acting for no process: no pid, so no pidfd, and no ids, as
    /// `SO_PEERCRED` reads with no peer.
    pub const KERNEL: Self = Self {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
        program: Program::Other,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_round_trips_through_its_word_and_zero_is_other() {
        assert_eq!(Program::from_raw(0), Program::Other);
        assert_eq!(Program::Other.to_raw(), 0);
        for raw in [1u64, 2, u64::MAX] {
            assert_eq!(Program::from_raw(raw).to_raw(), raw);
            assert!(matches!(Program::from_raw(raw), Program::Base(_)));
        }
    }
}
