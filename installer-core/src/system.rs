use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use slopos_boot_core::gpt;

use crate::{Issue, Layout};

/// What the install medium and the machine hold, as the engine read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct System {
    pub medium: Medium,
    pub disks: Vec<Disk>,
    /// The other systems the firmware's `BootOrder` lists that the new boot
    /// menu can offer, by title; or why they could not be read.
    pub others: Result<Vec<String>, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Medium {
    /// Whether the payload carries the toolchain for `/usr/local`.
    pub toolchain: bool,
    /// Whether the payload carries the clone for `/src`.
    pub source: bool,
    /// The payload volume's size, when it carries a tree an install takes.
    pub payload: Option<u64>,
    pub root_min: u64,
    /// The disk whose table names the medium's payload when the kernel did
    /// not mount it.
    pub unmounted_payload: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub node: String,
    pub state: DiskState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskState {
    /// Never offered, and why: the medium, or write-protected.
    Refused(String),
    /// Offered, but it could not be read.
    Unreadable(String),
    Listed(Listing),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub capacity: u64,
    pub block: u64,
    pub contents: Contents,
    /// Its GPT's layout; empty without one.
    pub layout: Layout,
    /// What each choice proposes, once the disk is opened.
    pub opened: Option<Opened>,
}

/// Whether choices on an opened disk propose an install, or what stops them.
pub type Fit = Result<(), Issue>;

/// What an opened disk takes, each choice proposed with every other answer
/// the engine's default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub erase: Fit,
    /// Free space, in the largest region.
    pub free: Fit,
    /// The largest region's index, which free space takes unless told.
    pub largest: Option<usize>,
    /// Free space, in each region by index.
    pub regions: Vec<Fit>,
    /// A partition as the root, the first that fits or else what stops the
    /// first.
    pub reuse: Fit,
    /// Each partition that may be the root, by number.
    pub roots: Vec<(u32, Fit)>,
    /// The SlopOS root the disk holds, installed over again.
    pub reinstall: Option<Reinstall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reinstall {
    pub root: u32,
    /// Whether it holds a root to keep, rather than one an install cut short
    /// before formatting it.
    pub keep: bool,
    pub fit: Fit,
}

/// What a disk holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contents {
    /// A GUID partition table the installer reads.
    Gpt,
    Blank,
    Mbr(Vec<MbrPartition>),
    /// A GUID partition table with more entries than SlopOS reads, or damaged
    /// in both copies.
    UnreadableGpt,
    Ext4,
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbrPartition {
    /// Its slot, from 0.
    pub index: u8,
    pub kind: u8,
    pub bytes: u64,
}

impl Contents {
    /// What erasing the disk loses, if anything.
    pub fn holds(&self) -> Option<String> {
        match self {
            Contents::Gpt => Some("a GUID partition table".into()),
            Contents::Blank => None,
            Contents::Mbr(parts) => Some(format!(
                "an MBR partition table with {} partition(s)",
                parts.len()
            )),
            Contents::UnreadableGpt => Some(format!(
                "a GUID partition table this installer cannot read: over {} entries, or damaged in both copies",
                gpt::MAX_ENTRIES
            )),
            Contents::Ext4 => Some("an ext4 volume across the whole disk".into()),
            Contents::Data => Some("data that starts no partition table".into()),
        }
    }

    /// What the disk holds, in words: `nothing` when it is blank.
    pub fn holding(&self) -> String {
        self.holds().unwrap_or_else(|| "nothing".into())
    }
}

impl System {
    pub fn disk(&self, node: &str) -> Option<&Disk> {
        self.disks.iter().find(|disk| disk.node == node)
    }
}

impl Disk {
    pub fn listing(&self) -> Option<&Listing> {
        match &self.state {
            DiskState::Listed(listing) => Some(listing),
            _ => None,
        }
    }

    pub fn opened(&self) -> Option<&Opened> {
        self.listing()?.opened.as_ref()
    }
}
