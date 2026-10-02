use super::*;
use crate::layout::{BOOT_TYPE, ESP_TYPE};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;
use std::{eprintln, format};

const DISK: Guid = Guid::from_spelling("0f0e0d0c-0b0a-4908-8706-050403020100");
const ESP_UUID: Guid = Guid::from_spelling("6d3c2a91-5b0e-4b6f-9a51-2f0c3e4d5a6b");
const BOOT_UUID: Guid = Guid::from_spelling("1a2b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d");

struct Part {
    slot: usize,
    type_guid: Guid,
    unique: Guid,
    first: u64,
    last: u64,
    name: &'static str,
}

/// A disk image with both table copies, laid out as §5.3 has a partitioning
/// tool write them: 128 entries of 128 bytes after each header.
fn disk(block: u64, blocks: u64, parts: &[Part]) -> Vec<u8> {
    disk_with(block, blocks, parts, 128, 2)
}

/// A disk whose arrays hold `entries` slots, the primary one at
/// `primary_array_lba` and the backup just below its header.
fn disk_with(
    block: u64,
    blocks: u64,
    parts: &[Part],
    entries: usize,
    primary_array_lba: u64,
) -> Vec<u8> {
    let mut image = vec![0u8; (block * blocks) as usize];
    let mut array = vec![0u8; entries * 128];
    let array_blocks = (array.len() as u64).div_ceil(block);
    for p in parts {
        let e = &mut array[p.slot * 128..(p.slot + 1) * 128];
        e[..16].copy_from_slice(&p.type_guid.0);
        e[16..32].copy_from_slice(&p.unique.0);
        e[32..40].copy_from_slice(&p.first.to_le_bytes());
        e[40..48].copy_from_slice(&p.last.to_le_bytes());
        for (unit, c) in e[56..].chunks_exact_mut(2).zip(p.name.encode_utf16()) {
            unit.copy_from_slice(&c.to_le_bytes());
        }
    }
    let array_crc = crc32::crc32(&array);
    let first_usable = primary_array_lba + array_blocks;
    let last_usable = blocks - 2 - array_blocks;
    let mut put = |my: u64, alternate: u64, entry_lba: u64| {
        let at = (entry_lba * block) as usize;
        image[at..at + array.len()].copy_from_slice(&array);
        let at = (my * block) as usize;
        let h = &mut image[at..at + 92];
        h[..8].copy_from_slice(SIGNATURE);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&DISK.0);
        h[72..80].copy_from_slice(&entry_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(entries as u32).to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32::crc32(h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
    };
    put(PRIMARY_LBA, blocks - 1, primary_array_lba);
    put(blocks - 1, PRIMARY_LBA, blocks - 1 - array_blocks);
    image
}

fn two_parts() -> [Part; 2] {
    [
        Part {
            slot: 0,
            type_guid: ESP_TYPE,
            unique: ESP_UUID,
            first: 2048,
            last: 4095,
            name: "EFI system partition",
        },
        Part {
            slot: 2,
            type_guid: BOOT_TYPE,
            unique: BOOT_UUID,
            first: 4096,
            last: 8191,
            name: "SlopOS boot",
        },
    ]
}

/// The table at `lba` of `image`, as a reader would take it.
fn read(image: &[u8], block: u64, lba: u64) -> Result<(Header, Vec<Partition>), Reject> {
    let geometry = Geometry::new(image.len() as u64, block).unwrap();
    let at = (lba * block) as usize;
    let header = Header::parse(&image[at..at + block as usize], lba, geometry)?;
    let at = (header.entry_lba() * block) as usize;
    let array = &image[at..at + header.array_bytes()];
    assert!(header.array_matches(array));
    let partitions = header.partitions(array).map(|p| p.unwrap()).collect();
    Ok((header, partitions))
}

/// What `partitions` makes of `parts` on a 512-byte disk, skips included.
fn partitions_of(parts: &[Part]) -> Vec<Result<(u32, u64, u64), Skipped>> {
    let image = disk(512, 16_384, parts);
    let geometry = Geometry::new(image.len() as u64, 512).unwrap();
    let header = Header::parse(&image[512..1024], PRIMARY_LBA, geometry).unwrap();
    let array = &image[1024..1024 + header.array_bytes()];
    header
        .partitions(array)
        .map(|p| p.map(|p| (p.entry.number, p.start, p.len)))
        .collect()
}

fn name_of(entry: &Entry) -> String {
    char::decode_utf16(entry.name.iter().copied().take_while(|&u| u != 0))
        .map(|c| c.unwrap())
        .collect()
}

#[test]
fn both_copies_parse_to_the_used_slots() {
    for block in [512, 4096] {
        let image = disk(block, 16_384, &two_parts());
        let blocks = image.len() as u64 / block;
        for lba in [PRIMARY_LBA, blocks - 1] {
            let (header, partitions) = read(&image, block, lba).unwrap();
            assert_eq!(header.disk_guid(), DISK);
            assert_eq!(partitions.len(), 2);
            let (esp, boot) = (partitions[0], partitions[1]);
            assert_eq!(esp.entry.number, 1);
            assert_eq!(esp.entry.unique, ESP_UUID);
            assert_eq!(name_of(&esp.entry), "EFI system partition");
            assert_eq!(boot.entry.number, 3, "an unused slot keeps its number");
            assert_eq!(boot.entry.type_guid, BOOT_TYPE);
            assert_eq!(boot.blocks(), 4096);
            assert_eq!((boot.start, boot.len), (4096 * block, 4096 * block));
        }
    }
}

/// A table may hold fewer slots than 128, and its primary array may lie
/// anywhere between its header and the first usable block.
#[test]
fn a_short_or_moved_array_reads_as_any_other() {
    for (block, entries, primary_array_lba) in [(4096, 4, 2), (512, 4, 2), (512, 128, 40)] {
        let parts = [Part {
            slot: 1,
            type_guid: BOOT_TYPE,
            unique: BOOT_UUID,
            first: 100,
            last: 199,
            name: "SlopOS boot",
        }];
        let image = disk_with(block, 4096, &parts, entries, primary_array_lba);
        for lba in [PRIMARY_LBA, 4095] {
            let (header, partitions) = read(&image, block, lba).unwrap();
            assert_eq!(header.array_bytes(), entries * 128);
            assert_eq!(partitions.len(), 1);
            assert_eq!(partitions[0].entry.number, 2);
            assert_eq!(partitions[0].start, 100 * block);
        }
    }
}

#[test]
fn a_damaged_header_is_corrupt_and_a_missing_one_absent() {
    let image = disk(512, 16_384, &two_parts());
    let geometry = Geometry::new(image.len() as u64, 512).unwrap();
    let header = |bytes: &[u8]| Header::parse(bytes, PRIMARY_LBA, geometry);
    let primary = &image[512..1024];
    assert!(header(primary).is_ok());

    let mut flipped = primary.to_vec();
    flipped[60] ^= 1;
    assert_eq!(header(&flipped), Err(Reject::Corrupt));
    assert_eq!(header(&[0u8; 512]), Err(Reject::Absent));
    assert_eq!(header(&primary[..64]), Err(Reject::Absent));
    assert_eq!(
        Header::parse(primary, 2, geometry),
        Err(Reject::Corrupt),
        "a copy read from where it does not say it lives"
    );
    let backup = &image[image.len() - 512..];
    assert_eq!(header(backup), Err(Reject::Corrupt));
}

fn resealed(mut header: Vec<u8>, edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
    edit(&mut header);
    header[16..20].fill(0);
    let crc = crc32::crc32(&header[..92]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    header
}

#[test]
fn what_this_reader_does_not_stage_is_unsupported_not_corrupt() {
    let image = disk(512, 16_384, &two_parts());
    let geometry = Geometry::new(image.len() as u64, 512).unwrap();
    let primary = image[512..1024].to_vec();
    let parse = |h: &[u8]| Header::parse(h, PRIMARY_LBA, geometry);
    let set_u32 = |at: usize, value: u32| {
        resealed(primary.clone(), |h| {
            h[at..at + 4].copy_from_slice(&value.to_le_bytes())
        })
    };
    assert_eq!(parse(&set_u32(8, 0x0002_0000)), Err(Reject::Unsupported));
    assert_eq!(parse(&set_u32(80, 129)), Err(Reject::Unsupported));
    assert_eq!(parse(&set_u32(80, 0)), Err(Reject::Unsupported));
    for stride in [0, 64, 132, 136, 192] {
        assert_eq!(
            parse(&set_u32(84, stride)),
            Err(Reject::Unsupported),
            "entry size {stride}"
        );
    }
}

#[test]
fn a_usable_range_reaching_the_table_is_corrupt() {
    let image = disk(512, 16_384, &two_parts());
    let geometry = Geometry::new(image.len() as u64, 512).unwrap();
    let primary = image[512..1024].to_vec();
    let usable = |first: u64, last: u64| {
        let header = resealed(primary.clone(), |h| {
            h[40..48].copy_from_slice(&first.to_le_bytes());
            h[48..56].copy_from_slice(&last.to_le_bytes());
        });
        Header::parse(&header, PRIMARY_LBA, geometry)
    };
    assert!(usable(34, 16_384 - 34).is_ok());
    for (first, last) in [(0, 0), (1, 100), (33, 100), (34, 16_384 - 33), (34, 16_383)] {
        assert_eq!(
            usable(first, last),
            Err(Reject::Corrupt),
            "{first}..={last}"
        );
    }
}

#[test]
fn a_device_too_short_for_its_table_is_corrupt() {
    let image = disk(512, 16_384, &two_parts());
    let short = Geometry::new(8_000 * 512, 512).unwrap();
    assert_eq!(
        Header::parse(&image[512..1024], PRIMARY_LBA, short),
        Err(Reject::Corrupt)
    );
    assert!(Geometry::new(1 << 20, 0).is_none());
    assert!(Geometry::new(1 << 20, 768).is_none());
}

#[test]
fn an_entry_outside_the_usable_range_is_skipped() {
    let mut parts = two_parts();
    parts[0].first = 1;
    assert_eq!(
        partitions_of(&parts)[0],
        Err(Skipped {
            number: 1,
            why: Skip::OutsideUsable
        })
    );
    parts[0].first = 5000;
    parts[0].last = 4000;
    assert_eq!(
        partitions_of(&parts)[0],
        Err(Skipped {
            number: 1,
            why: Skip::OutsideUsable
        })
    );
}

/// Only an entry that named a partition shuts out a later one.
#[test]
fn an_entry_overlapping_an_earlier_partition_is_skipped() {
    let part = |slot: usize, first, last| Part {
        slot,
        type_guid: BOOT_TYPE,
        unique: Guid([slot as u8 + 1; 16]),
        first,
        last,
        name: "",
    };
    let found = partitions_of(&[part(0, 100, 199), part(1, 150, 249), part(2, 200, 299)]);
    assert_eq!(found[0], Ok((1, 100 * 512, 100 * 512)));
    assert_eq!(
        found[1],
        Err(Skipped {
            number: 2,
            why: Skip::Overlaps
        })
    );
    assert_eq!(found[2], Ok((3, 200 * 512, 100 * 512)));
}

#[test]
fn a_damaged_array_does_not_match_its_header() {
    let mut image = disk(512, 16_384, &two_parts());
    let (header, _) = read(&image, 512, PRIMARY_LBA).unwrap();
    image[2 * 512 + 40] ^= 1;
    let array = &image[2 * 512..2 * 512 + header.array_bytes()];
    assert!(!header.array_matches(array));
    assert!(!header.array_matches(&array[..array.len() - 1]));
}

fn host_tool(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!("slopos-boot-core-{}", std::process::id()))
}

fn scratch(name: &str) -> PathBuf {
    fs::create_dir_all(scratch_dir()).unwrap();
    scratch_dir().join(name)
}

/// Partitions `img` as a disk of `block`-byte sectors from the sfdisk
/// `script`, through fdisk's `I` command: sfdisk takes a sector size only
/// from util-linux 2.40.
fn partition(img: &Path, block: u64, script: &str) {
    let script_path = img.with_extension("sfdisk");
    fs::write(&script_path, script).unwrap();
    let mut child = Command::new("fdisk")
        .arg("--sector-size")
        .arg(format!("{block}"))
        .arg(img)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let commands = format!("I\n{}\nw\n", script_path.display());
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), commands.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    let _ = fs::remove_file(&script_path);
}

/// A table fdisk wrote — on a 512-byte disk, and a short one on a 4K-native
/// disk — reads back with the partitions, GUIDs and names it was given.
#[test]
fn reads_what_fdisk_writes() {
    if !host_tool("fdisk") {
        eprintln!("skipped: needs fdisk");
        return;
    }
    for (block, table_length) in [(512u64, 128), (4096, 4)] {
        let img = scratch(&format!("disk-{block}.img"));
        let _ = fs::remove_file(&img);
        fs::File::create(&img).unwrap().set_len(64 << 20).unwrap();
        let mib = (1 << 20) / block;
        let script = format!(
            "label: gpt\nlabel-id: {DISK}\ntable-length: {}\n\
             start={}, size={}, type={ESP_TYPE}, uuid={ESP_UUID}, name=\"EFI system partition\"\n\
             start={}, size={}, type={BOOT_TYPE}, uuid={BOOT_UUID}, name=\"SlopOS boot\"\n",
            table_length,
            mib,
            8 * mib,
            9 * mib,
            16 * mib
        );
        partition(&img, block, &script);
        let image = fs::read(&img).unwrap();
        let blocks = image.len() as u64 / block;
        for lba in [PRIMARY_LBA, blocks - 1] {
            let (header, partitions) = read(&image, block, lba).unwrap();
            assert_eq!(header.disk_guid(), DISK);
            let found: Vec<_> = partitions
                .iter()
                .map(|p| {
                    let e = &p.entry;
                    (
                        e.number,
                        e.type_guid,
                        e.unique,
                        e.first_lba,
                        p.blocks(),
                        name_of(e),
                    )
                })
                .collect();
            assert_eq!(
                found,
                [
                    (
                        1,
                        ESP_TYPE,
                        ESP_UUID,
                        mib,
                        8 * mib,
                        String::from("EFI system partition")
                    ),
                    (
                        2,
                        BOOT_TYPE,
                        BOOT_UUID,
                        9 * mib,
                        16 * mib,
                        String::from("SlopOS boot")
                    ),
                ]
            );
        }
        let _ = fs::remove_file(&img);
    }
    let _ = fs::remove_dir(scratch_dir());
}

/// `header` and `array` written into `image` as the table write lays them
/// down, every piece of it, or only the first `pieces` of its write order.
fn write_table(image: &mut [u8], header: &Header, array: &[u8], pieces: usize) {
    let block = header.geometry().block() as usize;
    for piece in header.write_order().iter().take(pieces) {
        match *piece {
            Piece::Array(copy) => {
                let at = header.array_lba(copy) as usize * block;
                image[at..at + array.len()].copy_from_slice(array);
            }
            Piece::Header(copy) => {
                let lba = match copy {
                    Location::Primary => PRIMARY_LBA,
                    Location::Backup => header.geometry().backup_lba(),
                };
                let at = lba as usize * block;
                header.encode(copy, &mut image[at..at + block]);
            }
        }
    }
}

/// The table a reader takes: the primary, or the backup when the primary does
/// not read whole.
fn reader_view(image: &[u8], block: u64) -> Option<Vec<(u32, u64, u64)>> {
    let blocks = image.len() as u64 / block;
    [PRIMARY_LBA, blocks - 1].into_iter().find_map(|lba| {
        let geometry = Geometry::new(image.len() as u64, block).unwrap();
        let at = (lba * block) as usize;
        let header = Header::parse(&image[at..at + block as usize], lba, geometry).ok()?;
        let at = (header.entry_lba() * block) as usize;
        let array = &image[at..at + header.array_bytes()];
        header.array_matches(array).then(|| {
            header
                .partitions(array)
                .map(|p| p.unwrap())
                .map(|p| (p.entry.number, p.entry.first_lba, p.entry.last_lba))
                .collect()
        })
    })
}

fn put_entry(header: &Header, array: &mut [u8], number: u32, first: u64, last: u64) {
    let slot = header.slot_mut(array, number).unwrap();
    slot.fill(0);
    Entry {
        number,
        type_guid: BOOT_TYPE,
        unique: Guid([number as u8; 16]),
        first_lba: first,
        last_lba: last,
        attributes: 0,
        name: Entry::name_of("SlopOS boot"),
    }
    .encode(slot);
}

#[test]
fn a_new_table_reads_back_from_either_copy() {
    for (block, first_usable) in [(512u64, 34u64), (4096, 6)] {
        let blocks = 16_384;
        let mut image = vec![0u8; (block * blocks) as usize];
        let geometry = Geometry::new(image.len() as u64, block).unwrap();
        let header = Header::new(geometry, DISK).unwrap();
        let mut array = vec![0u8; header.array_bytes()];
        put_entry(&header, &mut array, 2, first_usable, first_usable + 99);
        let header = header.holding(&array);
        write_table(&mut image, &header, &array, 4);
        for lba in [PRIMARY_LBA, blocks - 1] {
            let (read, partitions) = read(&image, block, lba).unwrap();
            assert_eq!(read.disk_guid(), DISK);
            assert_eq!(read.first_usable(), first_usable);
            assert_eq!(read.last_usable(), blocks - first_usable);
            assert_eq!(read.entries(), NEW_ENTRIES);
            assert_eq!(partitions.len(), 1);
            assert_eq!(partitions[0].entry.number, 2);
            assert_eq!(name_of(&partitions[0].entry), "SlopOS boot");
        }
    }
}

#[test]
fn a_device_too_small_for_two_copies_has_no_new_table() {
    let geometry = Geometry::new(67 * 512, 512).unwrap();
    assert!(Header::new(geometry, DISK).is_none());
    let geometry = Geometry::new(68 * 512, 512).unwrap();
    assert!(Header::new(geometry, DISK).is_some());
}

/// Each step of the write leaves the old table or the new one whole.
#[test]
fn every_step_of_a_rewrite_leaves_one_whole_table() {
    let block = 512;
    let mut image = disk(block, 16_384, &two_parts());
    let (header, old) = read(&image, block, PRIMARY_LBA).unwrap();
    let old: Vec<_> = old
        .iter()
        .map(|p| (p.entry.number, p.entry.first_lba, p.entry.last_lba))
        .collect();
    let at = (header.entry_lba() * block) as usize;
    let mut array = image[at..at + header.array_bytes()].to_vec();
    put_entry(&header, &mut array, 2, 9000, 9999);
    let header = header.holding(&array);
    let mut new = old.clone();
    new.insert(1, (2, 9000, 9999));
    for pieces in 0..=4 {
        let mut step = image.clone();
        write_table(&mut step, &header, &array, pieces);
        let seen = reader_view(&step, block).expect("no whole table");
        assert!(
            seen == old || seen == new,
            "after {pieces} pieces: {seen:?}"
        );
    }
    write_table(&mut image, &header, &array, 4);
    assert_eq!(reader_view(&image, block).unwrap(), new);
}

/// A table read from its backup, the primary being damaged, is written
/// primary first: the backup it was read from stays whole until the primary
/// is, at every step.
#[test]
fn every_step_of_a_rewrite_from_the_backup_leaves_one_whole_table() {
    let block = 512;
    let mut image = disk(block, 16_384, &two_parts());
    image[block as usize..2 * block as usize].fill(0);
    let (header, old) = read(&image, block, 16_383).unwrap();
    let old: Vec<_> = old
        .iter()
        .map(|p| (p.entry.number, p.entry.first_lba, p.entry.last_lba))
        .collect();
    let at = (header.entry_lba() * block) as usize;
    let mut array = image[at..at + header.array_bytes()].to_vec();
    put_entry(&header, &mut array, 2, 9000, 9999);
    let header = header.holding(&array);
    let mut new = old.clone();
    new.insert(1, (2, 9000, 9999));
    for pieces in 0..=4 {
        let mut step = image.clone();
        write_table(&mut step, &header, &array, pieces);
        let seen = reader_view(&step, block).expect("no whole table");
        assert!(
            seen == old || seen == new,
            "after {pieces} pieces: {seen:?}"
        );
    }
}

#[test]
fn a_rewrite_keeps_every_other_slot_byte_for_byte() {
    let image = disk(512, 16_384, &two_parts());
    let (header, _) = read(&image, 512, PRIMARY_LBA).unwrap();
    let array = image[1024..1024 + header.array_bytes()].to_vec();
    let mut edited = array.clone();
    put_entry(&header, &mut edited, 5, 9000, 9999);
    for number in (1..=header.entries()).filter(|&n| n != 5) {
        assert_eq!(header.slot(&array, number), header.slot(&edited, number));
    }
    assert!(header.slot(&array, 0).is_none());
    assert!(header.slot(&array, header.entries() + 1).is_none());
}

/// A table read from its backup is written back with the primary's array
/// just after the primary header, and the backup's below the last block.
#[test]
fn a_table_read_from_its_backup_rewrites_both_copies() {
    let block = 4096;
    let mut image = disk(block, 16_384, &two_parts());
    image[block as usize..2 * block as usize].fill(0);
    assert!(read(&image, block, PRIMARY_LBA).is_err());
    let (header, partitions) = read(&image, block, 16_383).unwrap();
    assert_eq!(header.array_lba(Location::Primary), 2);
    assert_eq!(header.array_lba(Location::Backup), 16_383 - 4);
    let at = (header.entry_lba() * block) as usize;
    let array = image[at..at + header.array_bytes()].to_vec();
    write_table(&mut image, &header, &array, 4);
    let (_, again) = read(&image, block, PRIMARY_LBA).unwrap();
    assert_eq!(again, partitions);
}

#[test]
fn free_runs_are_the_usable_blocks_no_entry_reaches() {
    let part = |slot: usize, first, last| Part {
        slot,
        type_guid: BOOT_TYPE,
        unique: Guid([slot as u8 + 1; 16]),
        first,
        last,
        name: "",
    };
    let image = disk(512, 16_384, &[part(3, 8192, 10_239), part(0, 2048, 4095)]);
    let (header, _) = read(&image, 512, PRIMARY_LBA).unwrap();
    let array = &image[1024..1024 + header.array_bytes()];
    assert_eq!(
        header.free(array).runs(),
        [(34, 2047), (4096, 8191), (10_240, 16_350)]
    );
    let full = disk(512, 16_384, &[part(0, 34, 16_350)]);
    assert!(
        header
            .free(&full[1024..1024 + header.array_bytes()])
            .runs()
            .is_empty()
    );
    let overlapping = disk(512, 16_384, &[part(0, 100, 5000), part(1, 200, 300)]);
    let array = &overlapping[1024..1024 + header.array_bytes()];
    assert_eq!(header.free(array).runs(), [(34, 99), (5001, 16_350)]);
}

/// A disk that grew since its table was written: the table still reads, and
/// grown it offers the blocks up to the new end, which a rewrite then puts the
/// backup past.
#[test]
fn a_grown_disk_offers_what_it_gained() {
    let block = 512;
    let mut image = disk(block, 16_384, &two_parts());
    image.resize(2 * image.len(), 0);
    let (header, partitions) = read(&image, block, PRIMARY_LBA).unwrap();
    assert_eq!(header.last_usable(), 16_350);
    let grown = header.grown();
    assert_eq!(grown.last_usable(), 2 * 16_384 - 34);
    let at = (header.entry_lba() * block) as usize;
    let array = image[at..at + header.array_bytes()].to_vec();
    assert_eq!(
        grown.free(&array).runs().last(),
        Some(&(8192, 2 * 16_384 - 34))
    );
    write_table(&mut image, &grown, &array, 4);
    for lba in [PRIMARY_LBA, 2 * 16_384 - 1] {
        let (read, again) = read(&image, block, lba).unwrap();
        assert_eq!(read.last_usable(), 2 * 16_384 - 34);
        assert_eq!(again, partitions);
    }
}

#[test]
fn an_inverted_entry_holds_the_blocks_between_its_ends() {
    let part = |slot: usize, first, last| Part {
        slot,
        type_guid: BOOT_TYPE,
        unique: Guid([slot as u8 + 1; 16]),
        first,
        last,
        name: "",
    };
    let (header, _) = read(&disk(512, 16_384, &[]), 512, PRIMARY_LBA).unwrap();
    let image = disk(512, 16_384, &[part(0, 5000, 100)]);
    let array = &image[1024..1024 + header.array_bytes()];
    assert_eq!(header.free(array).runs(), [(34, 99), (5001, 16_350)]);
}

#[test]
fn the_protective_mbr_covers_the_disk_as_far_as_32_bits_reach() {
    let mut sector = [0xAAu8; 512];
    protective_mbr(Geometry::new(1 << 30, 512).unwrap(), &mut sector);
    assert_eq!(sector[446 + 4], 0xEE);
    assert_eq!(&sector[446 + 8..446 + 12], &1u32.to_le_bytes());
    assert_eq!(
        &sector[446 + 12..446 + 16],
        &((1u32 << 21) - 1).to_le_bytes()
    );
    assert_eq!(&sector[510..], &[0x55, 0xAA]);
    assert!(sector[..446].iter().all(|&b| b == 0));
    protective_mbr(Geometry::new(4 << 40, 512).unwrap(), &mut sector);
    assert_eq!(&sector[446 + 12..446 + 16], &u32::MAX.to_le_bytes());
}

/// What this writer puts down is a table sfdisk reads without complaint, with
/// the partitions it was given.
#[test]
fn sfdisk_reads_what_this_writes() {
    if !host_tool("sfdisk") {
        eprintln!("skipped: needs sfdisk");
        return;
    }
    let img = scratch("written.img");
    let blocks = (64u64 << 20) / 512;
    let mut image = vec![0u8; (blocks * 512) as usize];
    let geometry = Geometry::new(image.len() as u64, 512).unwrap();
    let mut mbr = [0u8; 512];
    protective_mbr(geometry, &mut mbr);
    image[..512].copy_from_slice(&mbr);
    let header = Header::new(geometry, DISK).unwrap();
    let mut array = vec![0u8; header.array_bytes()];
    put_entry(&header, &mut array, 1, 2048, 18_431);
    put_entry(&header, &mut array, 3, 18_432, 34_815);
    let header = header.holding(&array);
    write_table(&mut image, &header, &array, 4);
    fs::write(&img, &image).unwrap();
    let verify = Command::new("sfdisk")
        .arg("--verify")
        .arg(&img)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&verify.stdout).into_owned();
    assert!(verify.status.success(), "{said}");
    assert!(said.contains("No errors detected"), "{said}");
    let dump = Command::new("sfdisk")
        .arg("--dump")
        .arg(&img)
        .output()
        .unwrap();
    let dump: String = String::from_utf8_lossy(&dump.stdout)
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ") + "\n")
        .collect();
    assert!(dump.contains("label: gpt"), "{dump}");
    assert!(
        dump.contains(&format!("label-id: {}", DISK.to_string().to_uppercase())),
        "{dump}"
    );
    assert!(dump.contains("last-lba: 131038"), "{dump}");
    let line = |n: u32, start: u64, size: u64, unique: u8| {
        format!(
            "{}{n} : start= {start}, size= {size}, type={}, uuid={}, name=\"SlopOS boot\"",
            img.display(),
            BOOT_TYPE.to_string().to_uppercase(),
            Guid([unique; 16]).to_string().to_uppercase()
        )
    };
    assert!(dump.contains(&line(1, 2048, 16_384, 1)), "{dump}");
    assert!(dump.contains(&line(3, 18_432, 16_384, 3)), "{dump}");
    let _ = fs::remove_file(&img);
    let _ = fs::remove_dir(scratch_dir());
}

#[test]
fn a_name_is_cut_between_characters() {
    let text = format!("{}\u{1F600}", "a".repeat(NAME_UNITS - 1));
    let name = Entry::name_of(&text);
    assert_eq!(name[NAME_UNITS - 1], 0);
    assert_eq!(name[NAME_UNITS - 2], u16::from(b'a'));
    let fits = format!("{}\u{1F600}", "a".repeat(NAME_UNITS - 2));
    assert_eq!(String::from_utf16(&Entry::name_of(&fits)).unwrap(), fits);
}
