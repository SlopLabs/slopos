//! Disks, machines and proposals the host tests and `installer_ui_test`
//! share, built by the planner as the engine builds them.

use alloc::vec;
use alloc::vec::Vec;

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::{Entry, Geometry, Header};
use slopos_boot_core::install::{self, Mode as PlanMode, Plan, PlanError, RootSize};
use slopos_boot_core::layout::{BOOT_TYPE, CRASH_TYPE, ESP_NAME, ESP_TYPE, ROOT_TYPE};

use crate::{
    Contents, DEFAULT_CMDLINE, DEFAULT_REMOTE, Disk, DiskState, FIRST_PERSON, FirstPerson, Fit,
    Issue, Layout, Listing, Medium, Opened, Planned, Proposal, Reinstall, Setup, System, Table,
};

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const NODE: &str = "/dev/nvme0n1";
pub const LINUX: Guid = Guid::from_spelling("0fc63daf-8483-4772-8e79-3d69d8477de4");
const DISK: Guid = Guid::from_spelling("0f0e0d0c-0b0a-4908-8706-050403020100");
const UNIQUES: [Guid; 4] = [Guid([1; 16]), Guid([2; 16]), Guid([3; 16]), Guid([4; 16])];

/// A disk of `bytes` holding `parts`, each `(number, type, first, last)` in
/// MiB, named as an installer test's would be.
pub fn table(bytes: u64, parts: &[(u32, Guid, u64, u64)]) -> (Header, Vec<u8>) {
    let header = Header::new(Geometry::new(bytes, 512).unwrap(), DISK).unwrap();
    let mut array = vec![0u8; header.array_bytes()];
    let mib = MIB / 512;
    for &(number, type_guid, first, last) in parts {
        Entry {
            number,
            type_guid,
            unique: Guid([0xA0u8.wrapping_add(number as u8); 16]),
            first_lba: first * mib,
            last_lba: last * mib - 1,
            attributes: 0,
            name: Entry::name_of(if type_guid == ESP_TYPE {
                ESP_NAME
            } else {
                "theirs"
            }),
        }
        .encode(header.slot_mut(&mut array, number).unwrap());
    }
    (header.holding(&array), array)
}

/// An ESP and another system's partition on 16 GiB, the rest free.
pub fn foreign() -> (Header, Vec<u8>) {
    table(16 * GIB, &[(1, ESP_TYPE, 1, 101), (2, LINUX, 101, 613)])
}

/// An ESP and two of another system's partitions on 16 GiB, with three free
/// regions between and after them, the last too small for SlopOS.
pub fn gapped() -> (Header, Vec<u8>) {
    table(
        16 * GIB,
        &[
            (1, ESP_TYPE, 1, 101),
            (2, LINUX, 2048, 4096),
            (3, LINUX, 8192, 16000),
        ],
    )
}

/// SlopOS's four partitions on 16 GiB, the root third.
pub fn installed() -> (Header, Vec<u8>) {
    table(
        16 * GIB,
        &[
            (1, ESP_TYPE, 1, 261),
            (2, BOOT_TYPE, 261, 1285),
            (3, ROOT_TYPE, 1285, 9000),
            (4, CRASH_TYPE, 9000, 9004),
        ],
    )
}

pub fn setup() -> Setup {
    Setup {
        toolchain: true,
        remote: Some(DEFAULT_REMOTE.into()),
        person: Some(FirstPerson {
            name: FIRST_PERSON.into(),
            password: false,
        }),
        cmdline: DEFAULT_CMDLINE.into(),
        first: false,
        menu: vec!["Other OS".into(), "Elsewhere".into()],
        beside: vec!["Other OS".into()],
        left_out: Vec::new(),
        removed: Vec::new(),
    }
}

/// An install on [`NODE`], planned from the table it was read with.
pub struct Case {
    pub before: Option<(Header, Vec<u8>)>,
    pub contents: Contents,
    pub erase: bool,
    pub mode: PlanMode,
    pub size: RootSize,
    pub keep_root: bool,
    pub own_esp: bool,
}

impl Case {
    pub fn on(before: (Header, Vec<u8>), mode: PlanMode) -> Case {
        Case {
            before: Some(before),
            contents: Contents::Gpt,
            erase: false,
            mode,
            size: RootSize {
                min: GIB,
                target: None,
            },
            keep_root: false,
            own_esp: false,
        }
    }

    pub fn erasing(before: Option<(Header, Vec<u8>)>, contents: Contents) -> Case {
        Case {
            before,
            contents,
            erase: true,
            ..Case::on(table(16 * GIB, &[]), PlanMode::Free { region: None })
        }
    }

    pub fn plan(&self) -> (Plan, Header, Vec<u8>) {
        let (header, array) = match (&self.before, self.erase) {
            (Some((header, array)), false) => (header.grown(), array.clone()),
            (before, _) => {
                let capacity = before
                    .as_ref()
                    .map_or(16 * GIB, |(h, _)| h.geometry().capacity());
                let header = Header::new(Geometry::new(capacity, 512).unwrap(), DISK).unwrap();
                let array = vec![0u8; header.array_bytes()];
                (header, array)
            }
        };
        let plan = install::plan(&header, &array, self.mode, self.size).unwrap();
        let mut after = array.clone();
        let after_header = plan.apply(header, &mut after, UNIQUES);
        (plan, after_header, after)
    }

    pub fn proposal(&self) -> Proposal {
        let (plan, header, array) = self.plan();
        let before = self.before.as_ref().map(|(h, a)| Table {
            header: h,
            array: a,
        });
        Proposal::of(
            Planned {
                disk: NODE,
                contents: self.contents.clone(),
                capacity: header.geometry().capacity(),
                before,
                erase: self.erase,
                plan: &plan,
                after: Table {
                    header: &header,
                    array: &array,
                },
                format_esp: false,
                format_boot: false,
                keep_root: self.keep_root,
                own_esp: self.own_esp,
                root_min: self.size.min,
            },
            setup(),
        )
    }
}

pub fn no_room() -> Issue {
    Issue::Plan {
        error: PlanError::NoRoom {
            needed: 8 * GIB,
            available: GIB,
        },
        node: NODE.into(),
        root: None,
    }
}

/// What opening [`gapped`] answers when free space fits as `free` does: the
/// first two regions as `free`, the last too small, and partitions 2 and 3
/// as roots.
pub fn opened(free: Fit, reinstall: Option<Reinstall>) -> Opened {
    Opened {
        erase: Ok(()),
        free: free.clone(),
        largest: Some(1),
        regions: vec![free.clone(), free, Err(no_room())],
        reuse: Ok(()),
        roots: vec![(1, Err(no_room())), (2, Ok(())), (3, Ok(()))],
        reinstall,
    }
}

/// A machine whose medium carries the toolchain and the source: the medium
/// itself, [`NODE`] holding [`gapped`] and opened as `opened` says, and a
/// blank disk.
pub fn machine(opened: Option<Opened>) -> System {
    let (header, array) = gapped();
    System {
        medium: Medium {
            toolchain: true,
            source: true,
            payload: Some(GIB),
            root_min: 7 * GIB,
            unmounted_payload: None,
        },
        disks: vec![
            Disk {
                node: "/dev/sda".into(),
                state: DiskState::Refused("the install medium".into()),
            },
            Disk {
                node: NODE.into(),
                state: DiskState::Listed(Listing {
                    capacity: 16 * GIB,
                    block: 512,
                    contents: Contents::Gpt,
                    layout: Layout::of(NODE, &header, &array),
                    opened,
                }),
            },
            Disk {
                node: "/dev/vda".into(),
                state: DiskState::Listed(Listing {
                    capacity: 16 * GIB,
                    block: 512,
                    contents: Contents::Blank,
                    layout: Layout::empty(16 * GIB),
                    opened: None,
                }),
            },
        ],
        others: Ok(vec!["Other OS".into()]),
    }
}
