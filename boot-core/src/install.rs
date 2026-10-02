//! Where an install puts SlopOS's partitions on a disk that may hold another
//! system's: into one run of free blocks, beside what is there, or over a
//! partition the user names as the root. Nothing the plan does not name is
//! moved, and an existing ESP is shared rather than replaced.
//!
//! A disk being erased is planned as a fresh table, [`Header::new`], with
//! nothing used.

use crate::gpt::{Entry, Header};
use crate::guid::Guid;
use crate::layout::{
    ALIGN_BYTES, BOOT_BYTES, BOOT_NAME, BOOT_TYPE, CRASH_BYTES, CRASH_NAME, CRASH_TYPE, ESP_NAME,
    ESP_TYPE, NEW_ESP_BYTES, ROOT_MAX_BYTES, ROOT_NAME, ROOT_TYPE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Esp,
    Boot,
    Root,
    Crash,
}

/// An entry's attribute bits 48 to 63, which each partition type defines for
/// itself (UEFI 2.10, table 5.8).
const TYPE_SPECIFIC_ATTRIBUTES: u64 = 0xFFFF << 48;

/// In the order a new run lays them out.
pub const ROLES: [Role; 4] = [Role::Esp, Role::Boot, Role::Root, Role::Crash];

impl Role {
    pub fn type_guid(self) -> Guid {
        match self {
            Role::Esp => ESP_TYPE,
            Role::Boot => BOOT_TYPE,
            Role::Root => ROOT_TYPE,
            Role::Crash => CRASH_TYPE,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Esp => ESP_NAME,
            Role::Boot => BOOT_NAME,
            Role::Root => ROOT_NAME,
            Role::Crash => CRASH_NAME,
        }
    }

    /// What a new partition of this role takes; the root takes the rest.
    fn bytes(self) -> u64 {
        match self {
            Role::Esp => NEW_ESP_BYTES,
            Role::Boot => BOOT_BYTES,
            Role::Root => 0,
            Role::Crash => CRASH_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every partition SlopOS needs, new, in the free region of that index in
    /// [`regions`], or the largest; the disk's ESP if it has one.
    Free { region: Option<usize> },
    /// The root over the partition numbered `root`, whatever it held; the
    /// disk's ESP and SlopOS's boot and crash partitions where it has them,
    /// and new ones in the region otherwise.
    Reuse { root: u32, region: Option<usize> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// The partition of this number, as it is.
    Existing(u32),
    /// A new partition in slot `number`, over the inclusive LBA range.
    New {
        number: u32,
        first_lba: u64,
        last_lba: u64,
    },
}

impl Place {
    pub fn number(self) -> u32 {
        match self {
            Place::Existing(number) | Place::New { number, .. } => number,
        }
    }

    pub fn is_new(self) -> bool {
        matches!(self, Place::New { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub esp: Place,
    pub boot: Place,
    pub root: Place,
    pub crash: Place,
}

impl Plan {
    pub fn place(&self, role: Role) -> Place {
        match role {
            Role::Esp => self.esp,
            Role::Boot => self.boot,
            Role::Root => self.root,
            Role::Crash => self.crash,
        }
    }

    /// Write the plan into `array`, the table `header` reads it with, and
    /// answer the header for the array as written. A new partition takes its
    /// role's GUID from `uniques`, and so does a root taken from another
    /// system, shedding its old type's attribute bits so nothing that named it
    /// finds it; a SlopOS root keeps its own.
    pub fn apply(&self, header: Header, array: &mut [u8], uniques: [Guid; 4]) -> Header {
        for (role, unique) in ROLES.into_iter().zip(uniques) {
            let place = self.place(role);
            let entry = match place {
                Place::New {
                    number,
                    first_lba,
                    last_lba,
                } => Entry {
                    number,
                    type_guid: role.type_guid(),
                    unique,
                    first_lba,
                    last_lba,
                    attributes: 0,
                    name: Entry::name_of(role.name()),
                },
                Place::Existing(number) if role == Role::Root => {
                    let Some(mut entry) = header
                        .partitions(array)
                        .filter_map(Result::ok)
                        .map(|p| p.entry)
                        .find(|e| e.number == number)
                    else {
                        continue;
                    };
                    if entry.type_guid != ROOT_TYPE {
                        entry.unique = unique;
                        entry.attributes &= !TYPE_SPECIFIC_ATTRIBUTES;
                    }
                    entry.type_guid = ROOT_TYPE;
                    entry.name = Entry::name_of(ROOT_NAME);
                    entry
                }
                Place::Existing(_) => continue,
            };
            if let Some(slot) = header.slot_mut(array, entry.number) {
                if place.is_new() {
                    slot.fill(0);
                }
                entry.encode(slot);
            }
        }
        header.holding(array)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// No free region of that index, or none at all.
    NoRegion,
    /// The region holds `available` bytes once aligned; the new partitions
    /// need `needed`.
    NoRoom {
        needed: u64,
        available: u64,
    },
    NoPartition(u32),
    /// The ESP, or a SlopOS boot or crash partition, cannot be the root.
    NotForRoot(u32),
    /// A free-space install onto a disk SlopOS is already on: a second boot
    /// partition would leave which one boots unknown.
    Installed(u32),
    /// Two partitions of a role there may be one of.
    Ambiguous(Role),
    /// The root named holds fewer bytes than the system needs.
    RootTooSmall {
        needed: u64,
        available: u64,
    },
    /// The root named holds more bytes than a root may: [`ROOT_MAX_BYTES`].
    RootTooLarge {
        limit: u64,
        available: u64,
    },
    /// A reuse names a root while SlopOS's own is this other partition: a
    /// second would leave which one boots unknown.
    RootElsewhere(u32),
    /// The table has no unused slot for a new partition.
    NoSlot,
}

/// A run of free blocks with its ends moved inward to [`ALIGN_BYTES`]
/// boundaries: where a new partition may start and end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub first_lba: u64,
    pub last_lba: u64,
}

impl Region {
    pub fn blocks(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

fn align_blocks(header: &Header) -> u64 {
    ALIGN_BYTES / header.geometry().block()
}

/// The free runs of `array` that hold at least one aligned unit, lowest first.
pub fn regions(header: &Header, array: &[u8]) -> impl Iterator<Item = Region> + use<> {
    let align = align_blocks(header);
    let free = header.free(array);
    let mut index = 0;
    core::iter::from_fn(move || {
        while let Some(&(first, last)) = free.runs().get(index) {
            index += 1;
            let start = first.div_ceil(align) * align;
            let end = (last + 1) / align * align;
            if end > start {
                return Some(Region {
                    first_lba: start,
                    last_lba: end - 1,
                });
            }
        }
        None
    })
}

fn region(header: &Header, array: &[u8], index: Option<usize>) -> Result<Region, PlanError> {
    let mut all = regions(header, array);
    match index {
        Some(index) => all.nth(index),
        None => all.fold(None, |best: Option<Region>, r| match best {
            Some(b) if b.blocks() >= r.blocks() => Some(b),
            _ => Some(r),
        }),
    }
    .ok_or(PlanError::NoRegion)
}

/// The one valid partition of `role`'s type, the first of several ESPs, or
/// none.
fn existing(header: &Header, array: &[u8], role: Role) -> Result<Option<u32>, PlanError> {
    let mut found = header
        .partitions(array)
        .filter_map(Result::ok)
        .filter(|p| p.entry.type_guid == role.type_guid());
    let first = found.next().map(|p| p.entry.number);
    if role != Role::Esp && first.is_some() && found.next().is_some() {
        return Err(PlanError::Ambiguous(role));
    }
    Ok(first)
}

/// Lay out a SlopOS install on the disk whose table is `header` and `array`,
/// with a root of at least `root_min` bytes.
pub fn plan(header: &Header, array: &[u8], mode: Mode, root_min: u64) -> Result<Plan, PlanError> {
    let block = header.geometry().block();
    let mut wanted = [None; 4];
    let reused_root = match mode {
        Mode::Free { .. } => {
            for role in [Role::Boot, Role::Root, Role::Crash] {
                if let Some(number) = existing(header, array, role)? {
                    return Err(PlanError::Installed(number));
                }
            }
            None
        }
        Mode::Reuse { root, .. } => {
            let named = header
                .partitions(array)
                .filter_map(Result::ok)
                .find(|p| p.entry.number == root)
                .ok_or(PlanError::NoPartition(root))?;
            if [ESP_TYPE, BOOT_TYPE, CRASH_TYPE].contains(&named.entry.type_guid) {
                return Err(PlanError::NotForRoot(root));
            }
            if named.len < root_min {
                return Err(PlanError::RootTooSmall {
                    needed: root_min,
                    available: named.len,
                });
            }
            if named.len > ROOT_MAX_BYTES {
                return Err(PlanError::RootTooLarge {
                    limit: ROOT_MAX_BYTES,
                    available: named.len,
                });
            }
            if let Some(own) = existing(header, array, Role::Root)?.filter(|&own| own != root) {
                return Err(PlanError::RootElsewhere(own));
            }
            for role in [Role::Boot, Role::Crash] {
                wanted[role as usize] = existing(header, array, role)?.map(Place::Existing);
            }
            wanted[Role::Root as usize] = Some(Place::Existing(root));
            Some(root)
        }
    };
    wanted[Role::Esp as usize] = existing(header, array, Role::Esp)?.map(Place::Existing);
    let fixed: u64 = ROLES
        .into_iter()
        .filter(|&role| wanted[role as usize].is_none())
        .map(Role::bytes)
        .sum();
    let needed = fixed + if reused_root.is_some() { 0 } else { root_min };
    let new_count = wanted.iter().filter(|w| w.is_none()).count();
    let region = if new_count == 0 {
        None
    } else {
        let index = match mode {
            Mode::Free { region } | Mode::Reuse { region, .. } => region,
        };
        let region = region(header, array, index)?;
        if region.blocks() * block < needed {
            return Err(PlanError::NoRoom {
                needed,
                available: region.blocks() * block,
            });
        }
        Some(region)
    };

    let mut taken = [0u32; 4];
    let mut next_lba = region.map_or(0, |r| r.first_lba);
    for role in ROLES {
        if wanted[role as usize].is_some() {
            continue;
        }
        let region = region.ok_or(PlanError::NoRegion)?;
        let number = unused_slot_after(header, array, &taken).ok_or(PlanError::NoSlot)?;
        taken[role as usize] = number;
        let blocks = match role {
            Role::Root => {
                let after = if wanted[Role::Crash as usize].is_none() {
                    CRASH_BYTES / block
                } else {
                    0
                };
                (region.last_lba + 1 - next_lba - after).min(ROOT_MAX_BYTES / block)
            }
            role => role.bytes() / block,
        };
        wanted[role as usize] = Some(Place::New {
            number,
            first_lba: next_lba,
            last_lba: next_lba + blocks - 1,
        });
        next_lba += blocks;
    }
    let [esp, boot, root, crash] = wanted.map(|place| place.expect("every role is placed"));
    Ok(Plan {
        esp,
        boot,
        root,
        crash,
    })
}

/// The lowest slot of `array` no entry uses that `taken` does not hold.
fn unused_slot_after(header: &Header, array: &[u8], taken: &[u32; 4]) -> Option<u32> {
    (1..=header.entries()).find(|&number| {
        !taken.contains(&number)
            && header
                .slot(array, number)
                .is_some_and(|slot| slot[..16] == [0; 16])
    })
}

#[cfg(test)]
mod tests;
