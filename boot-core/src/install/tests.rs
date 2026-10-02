use super::*;
use crate::gpt::{Geometry, Header};
use std::vec;
use std::vec::Vec;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const DISK: Guid = Guid::from_spelling("0f0e0d0c-0b0a-4908-8706-050403020100");
const OTHER: Guid = Guid::from_spelling("0fc63daf-8483-4772-8e79-3d69d8477de4");
const UNIQUES: [Guid; 4] = [Guid([1; 16]), Guid([2; 16]), Guid([3; 16]), Guid([4; 16])];

/// A table on a disk of `bytes` holding `parts`, each `(number, type, first,
/// last)` in LBAs.
fn table(bytes: u64, block: u64, parts: &[(u32, Guid, u64, u64)]) -> (Header, Vec<u8>) {
    let header = Header::new(Geometry::new(bytes, block).unwrap(), DISK).unwrap();
    let mut array = vec![0u8; header.array_bytes()];
    for &(number, type_guid, first_lba, last_lba) in parts {
        Entry {
            number,
            type_guid,
            unique: Guid([0xA0u8.wrapping_add(number as u8); 16]),
            first_lba,
            last_lba,
            attributes: 0,
            name: Entry::name_of("theirs"),
        }
        .encode(header.slot_mut(&mut array, number).unwrap());
    }
    (header.holding(&array), array)
}

fn new(place: Place) -> (u32, u64, u64) {
    match place {
        Place::New {
            number,
            first_lba,
            last_lba,
        } => (number, first_lba, last_lba),
        Place::Existing(n) => panic!("partition {n} kept, not new"),
    }
}

#[test]
fn an_erased_disk_gets_esp_boot_root_and_crash_in_that_order() {
    for block in [512u64, 4096] {
        let bytes = 16 * GIB;
        let (header, array) = table(bytes, block, &[]);
        let plan = plan(&header, &array, Mode::Free { region: None }, GIB).unwrap();
        let mib = MIB / block;
        let last = bytes / block - 1;
        assert_eq!(new(plan.esp), (1, mib, 261 * mib - 1));
        assert_eq!(new(plan.boot), (2, 261 * mib, 1285 * mib - 1));
        let (_, crash_first, crash_last) = new(plan.crash);
        assert_eq!(crash_last + 1, (last + 1 - mib), "the backup's MiB is left");
        assert_eq!(crash_last + 1 - crash_first, 4 * mib);
        assert_eq!(new(plan.root), (3, 1285 * mib, crash_first - 1));
        assert_eq!(plan.crash.number(), 4);
    }
}

#[test]
fn a_shared_disk_keeps_its_esp_and_fills_the_largest_region() {
    let mib = MIB / 512;
    let (header, array) = table(
        32 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 101 * mib - 1),
            (2, OTHER, 101 * mib, 8293 * mib - 1),
        ],
    );
    let plan = plan(&header, &array, Mode::Free { region: None }, 4 * GIB).unwrap();
    assert_eq!(plan.esp, Place::Existing(1));
    assert_eq!(new(plan.boot), (3, 8293 * mib, 9317 * mib - 1));
    assert_eq!(new(plan.root).0, 4);
    assert_eq!(plan.crash.number(), 5);
}

#[test]
fn a_region_is_chosen_by_index_and_aligned_inward() {
    let mib = MIB / 512;
    let (header, array) = table(
        16 * GIB,
        512,
        &[(1, OTHER, 3 * GIB / 512 + 7, 4 * GIB / 512 - 1)],
    );
    let found: Vec<Region> = regions(&header, &array).collect();
    assert_eq!(found[0].first_lba, mib);
    assert_eq!(found[0].last_lba, 3 * GIB / 512 - 1);
    assert_eq!(found[1].first_lba, 4 * GIB / 512);
    let first = plan(&header, &array, Mode::Free { region: Some(0) }, GIB).unwrap();
    assert_eq!(new(first.esp).1, mib);
    let (_, _, crash_last) = new(first.crash);
    assert_eq!(crash_last, 3 * GIB / 512 - 1);
    let largest = plan(&header, &array, Mode::Free { region: None }, GIB).unwrap();
    assert_eq!(new(largest.esp).1, 4 * GIB / 512);
    assert_eq!(
        plan(&header, &array, Mode::Free { region: Some(2) }, GIB),
        Err(PlanError::NoRegion)
    );
}

#[test]
fn a_region_too_small_is_refused_with_what_it_lacks() {
    let (header, array) = table(2 * GIB, 512, &[]);
    let Err(PlanError::NoRoom { needed, available }) =
        plan(&header, &array, Mode::Free { region: None }, GIB)
    else {
        panic!("a 2 GiB disk holds no 1 GiB root beside a 1 GiB boot partition");
    };
    assert_eq!(needed, NEW_ESP_BYTES + BOOT_BYTES + CRASH_BYTES + GIB);
    assert!(available < needed);
}

#[test]
fn free_space_on_a_disk_slopos_is_on_is_refused() {
    let mib = MIB / 512;
    let (header, array) = table(16 * GIB, 512, &[(2, BOOT_TYPE, mib, 1025 * mib - 1)]);
    assert_eq!(
        plan(&header, &array, Mode::Free { region: None }, GIB),
        Err(PlanError::Installed(2))
    );
}

#[test]
fn a_reused_root_takes_the_existing_esp_and_new_boot_and_crash() {
    let mib = MIB / 512;
    let (header, array) = table(
        16 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 101 * mib - 1),
            (2, OTHER, 101 * mib, 101 * mib + 4 * GIB / 512 - 1),
        ],
    );
    let mode = Mode::Reuse {
        root: 2,
        region: None,
    };
    let plan = plan(&header, &array, mode, 2 * GIB).unwrap();
    assert_eq!(plan.esp, Place::Existing(1));
    assert_eq!(plan.root, Place::Existing(2));
    let (boot, boot_first, boot_last) = new(plan.boot);
    assert_eq!(boot, 3);
    assert_eq!(boot_first, 101 * mib + 4 * GIB / 512);
    assert_eq!(new(plan.crash), (4, boot_last + 1, boot_last + 4 * mib));
}

#[test]
fn a_reinstall_keeps_every_slopos_partition() {
    let mib = MIB / 512;
    let (header, array) = table(
        16 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 261 * mib - 1),
            (2, BOOT_TYPE, 261 * mib, 1285 * mib - 1),
            (3, ROOT_TYPE, 1285 * mib, 9000 * mib - 1),
            (4, CRASH_TYPE, 9000 * mib, 9004 * mib - 1),
        ],
    );
    let full = (header, array);
    let mode = Mode::Reuse {
        root: 3,
        region: None,
    };
    let plan = plan(&full.0, &full.1, mode, GIB).unwrap();
    assert_eq!(
        [plan.esp, plan.boot, plan.root, plan.crash],
        [1, 2, 3, 4].map(Place::Existing)
    );
}

#[test]
fn what_cannot_be_a_root_is_refused() {
    let mib = MIB / 512;
    let (header, array) = table(
        16 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 101 * mib - 1),
            (2, OTHER, 101 * mib, 102 * mib - 1),
            (3, BOOT_TYPE, 102 * mib, 1126 * mib - 1),
            (4, BOOT_TYPE, 1126 * mib, 2150 * mib - 1),
        ],
    );
    let reuse = |root| plan(&header, &array, Mode::Reuse { root, region: None }, 2 * MIB);
    assert_eq!(reuse(1), Err(PlanError::NotForRoot(1)));
    assert_eq!(reuse(3), Err(PlanError::NotForRoot(3)));
    assert_eq!(reuse(9), Err(PlanError::NoPartition(9)));
    assert_eq!(
        reuse(2),
        Err(PlanError::RootTooSmall {
            needed: 2 * MIB,
            available: MIB
        })
    );
    let roomy = plan(
        &header,
        &array,
        Mode::Reuse {
            root: 2,
            region: None,
        },
        MIB,
    );
    assert_eq!(roomy, Err(PlanError::Ambiguous(Role::Boot)));
}

#[test]
fn a_root_stops_where_the_kernel_can_number_its_blocks() {
    let block = 4096;
    let (header, array) = table(20 << 40, block, &[]);
    let plan = plan(&header, &array, Mode::Free { region: None }, GIB).unwrap();
    let (_, first, last) = new(plan.root);
    assert_eq!((last - first + 1) * block, ROOT_MAX_BYTES);
    assert_eq!(new(plan.crash).1, last + 1);
}

#[test]
fn a_full_array_has_no_slot_for_a_new_partition() {
    let mib = MIB / 512;
    let parts: Vec<(u32, Guid, u64, u64)> = (1..=128)
        .map(|n| (n, OTHER, mib * u64::from(n), mib * u64::from(n) + 1))
        .collect();
    let (header, array) = table(16 * GIB, 512, &parts);
    assert_eq!(
        plan(&header, &array, Mode::Free { region: None }, GIB),
        Err(PlanError::NoSlot)
    );
}

#[test]
fn applying_a_plan_writes_the_new_entries_and_retypes_a_reused_root() {
    let mib = MIB / 512;
    let (header, mut array) = table(
        16 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 101 * mib - 1),
            (2, OTHER, 101 * mib, 5000 * mib - 1),
        ],
    );
    let before = header.slot(&array, 1).unwrap().to_vec();
    let mode = Mode::Reuse {
        root: 2,
        region: None,
    };
    let plan = plan(&header, &array, mode, GIB).unwrap();
    let header = plan.apply(header, &mut array, UNIQUES);
    assert!(header.array_matches(&array));
    assert_eq!(header.slot(&array, 1).unwrap(), &before[..]);
    let found: Vec<_> = header
        .partitions(&array)
        .map(|p| p.unwrap().entry)
        .map(|e| (e.number, e.type_guid, e.unique))
        .collect();
    assert_eq!(
        found,
        [
            (1, ESP_TYPE, Guid([0xA1; 16])),
            (2, ROOT_TYPE, UNIQUES[2]),
            (3, BOOT_TYPE, UNIQUES[1]),
            (4, CRASH_TYPE, UNIQUES[3]),
        ]
    );
    let root = header.partitions(&array).nth(1).unwrap().unwrap();
    assert_eq!(
        (root.entry.first_lba, root.entry.last_lba),
        (101 * mib, 5000 * mib - 1)
    );
    assert_eq!(root.entry.name, Entry::name_of(ROOT_NAME));
}

/// A root taken from another system sheds the bits its old type defined; one
/// SlopOS already had keeps its unique GUID, which its slots boot it by.
#[test]
fn a_reused_root_is_new_to_the_disk_unless_it_was_slopos_s() {
    let mib = MIB / 512;
    for (old_type, kept) in [(OTHER, false), (ROOT_TYPE, true)] {
        let (header, mut array) = table(
            16 * GIB,
            512,
            &[
                (1, ESP_TYPE, mib, 101 * mib - 1),
                (2, old_type, 101 * mib, 5000 * mib - 1),
            ],
        );
        let slot = header.slot_mut(&mut array, 2).unwrap();
        slot[48..56].copy_from_slice(&(0xFFFF_0000_0000_0001u64).to_le_bytes());
        let header = header.holding(&array);
        let mode = Mode::Reuse {
            root: 2,
            region: None,
        };
        let plan = plan(&header, &array, mode, GIB).unwrap();
        let header = plan.apply(header, &mut array, UNIQUES);
        let root = header.partitions(&array).nth(1).unwrap().unwrap().entry;
        if kept {
            assert_eq!(root.unique, Guid([0xA2; 16]));
            assert_eq!(root.attributes, 0xFFFF_0000_0000_0001);
        } else {
            assert_eq!(root.unique, UNIQUES[2]);
            assert_eq!(root.attributes, 1);
        }
    }
}

/// A reuse that names another partition while SlopOS's root is on the disk is
/// refused, naming the root it would leave beside a second one.
#[test]
fn a_reuse_beside_slopos_s_own_root_is_refused() {
    let mib = MIB / 512;
    let (header, array) = table(
        16 * GIB,
        512,
        &[
            (1, ESP_TYPE, mib, 101 * mib - 1),
            (2, ROOT_TYPE, 101 * mib, 5000 * mib - 1),
            (3, OTHER, 5000 * mib, 10_000 * mib - 1),
        ],
    );
    let reuse = |root| plan(&header, &array, Mode::Reuse { root, region: None }, GIB);
    assert_eq!(reuse(3), Err(PlanError::RootElsewhere(2)));
    assert!(reuse(2).is_ok());
}
