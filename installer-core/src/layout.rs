use alloc::string::String;
use alloc::vec::Vec;

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::Header;
use slopos_boot_core::install::{self, ROLES, Role};

use crate::{name_of, partition_node, type_name};

/// What a disk's table lays out, lowest first: its partitions and the free
/// regions a new partition may go into. Bytes between them hold the table
/// itself, or are too small to align a partition in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub capacity: u64,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub bytes: u64,
    pub piece: Piece,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Partition {
        number: u32,
        node: String,
        kind: String,
        name: String,
        /// What SlopOS takes it for, by its type.
        role: Option<Role>,
    },
    /// The free region of this index, as [`crate::Mode`] names one.
    Free { region: usize },
}

fn role_of(type_guid: &Guid) -> Option<Role> {
    ROLES
        .into_iter()
        .find(|role| role.type_guid() == *type_guid)
}

impl Layout {
    /// A disk with no table this reader reads.
    pub fn empty(capacity: u64) -> Layout {
        Layout {
            capacity,
            segments: Vec::new(),
        }
    }

    pub fn of(disk: &str, header: &Header, array: &[u8]) -> Layout {
        let block = header.geometry().block();
        let partitions = header
            .partitions(array)
            .filter_map(Result::ok)
            .map(|p| Segment {
                start: p.start,
                bytes: p.len,
                piece: Piece::Partition {
                    number: p.entry.number,
                    node: partition_node(disk, p.entry.number),
                    kind: type_name(&p.entry.type_guid),
                    name: name_of(&p),
                    role: role_of(&p.entry.type_guid),
                },
            });
        let free = install::regions(header, array)
            .enumerate()
            .map(|(region, r)| Segment {
                start: r.first_lba * block,
                bytes: r.blocks() * block,
                piece: Piece::Free { region },
            });
        let mut segments: Vec<Segment> = partitions.chain(free).collect();
        segments.sort_by_key(|segment| segment.start);
        Layout {
            capacity: header.geometry().capacity(),
            segments,
        }
    }

    /// The free regions, by index.
    pub fn regions(&self) -> impl Iterator<Item = &Segment> {
        self.segments
            .iter()
            .filter(|segment| matches!(segment.piece, Piece::Free { .. }))
    }
}
