use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use super::read::{Blocks, Found, Volume, VolumeError};
use super::write::{Entry, What, layout};
use super::{BLOCK, Kind};

const UUID: [u8; 16] = *b"slopos-ext-tests";

struct Memory(Vec<u8>);

impl Blocks for Memory {
    type Error = ();

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), ()> {
        let at = block as usize * BLOCK;
        out.copy_from_slice(self.0.get(at..at + BLOCK).ok_or(())?);
        Ok(())
    }
}

struct File(std::fs::File);

impl Blocks for File {
    type Error = std::io::Error;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.0.read_exact_at(out, block * BLOCK as u64)
    }
}

fn file(path: &str, perm: u16, bytes: &[u8]) -> (Entry, Vec<u8>) {
    let entry = Entry {
        path: path.into(),
        perm,
        what: What::File(bytes.len() as u64),
    };
    (entry, bytes.to_vec())
}

fn symlink(path: &str, target: &str) -> (Entry, Vec<u8>) {
    let entry = Entry {
        path: path.into(),
        perm: 0o777,
        what: What::Symlink(target.as_bytes().to_vec()),
    };
    (entry, Vec::new())
}

fn dir(path: &str) -> (Entry, Vec<u8>) {
    let entry = Entry {
        path: path.into(),
        perm: 0o750,
        what: What::Directory,
    };
    (entry, Vec::new())
}

/// The volume of `tree`, in memory.
fn build(tree: &[(Entry, Vec<u8>)]) -> Vec<u8> {
    let entries: Vec<Entry> = tree.iter().map(|(e, _)| e.clone()).collect();
    let layout = layout(&entries, UUID).unwrap();
    let mut out = Vec::new();
    layout
        .write::<()>(
            &mut |i, offset, room| {
                let at = offset as usize;
                room.copy_from_slice(&tree[i].1[at..at + room.len()]);
                Ok(())
            },
            &mut |block| {
                out.extend_from_slice(block);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(out.len() as u64, layout.blocks() * BLOCK as u64);
    out
}

fn sample() -> Vec<(Entry, Vec<u8>)> {
    let big: Vec<u8> = (0..3 * BLOCK + 17).map(|i| (i * 31 % 251) as u8).collect();
    let long = "x/".repeat(40) + "end";
    vec![
        file("/bin/tool", 0o755, b"#!/bin/sh\necho tool\n"),
        file("/lib/libx.so", 0o644, &big),
        file("/share/empty", 0o644, b""),
        file("/share/one", 0o600, &[7; BLOCK]),
        symlink("/lib/libx.so.1", "libx.so"),
        symlink("/share/long", &long),
        dir("/share/nested"),
        file("/share/nested/deep/leaf", 0o644, b"leaf"),
    ]
}

fn contents(volume: &mut Volume<Memory>, found: &Found) -> Vec<u8> {
    let inode = volume.inode(found.ino).unwrap();
    volume.read_all(&inode, u64::MAX).unwrap()
}

#[test]
fn a_tree_reads_back_as_written() {
    let tree = sample();
    let image = build(&tree);
    let blocks = (image.len() / BLOCK) as u64;
    let mut volume = Volume::open(Memory(image), blocks).unwrap();
    let found = volume.walk(100, 1 << 24).unwrap();
    let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/bin",
            "/lib",
            "/lost+found",
            "/share",
            "/bin/tool",
            "/lib/libx.so",
            "/lib/libx.so.1",
            "/share/empty",
            "/share/long",
            "/share/nested",
            "/share/one",
            "/share/nested/deep",
            "/share/nested/deep/leaf",
        ]
    );
    let by_path: BTreeMap<&str, &Found> = found.iter().map(|f| (f.path.as_str(), f)).collect();
    for (entry, bytes) in &tree {
        let got = by_path[entry.path.as_str()];
        assert_eq!(got.mode & 0o7777, entry.perm, "{}", entry.path);
        match &entry.what {
            What::File(_) => {
                assert_eq!(got.kind, Kind::File);
                assert_eq!(&contents(&mut volume, got), bytes, "{}", entry.path);
            }
            What::Symlink(target) => {
                assert_eq!(got.kind, Kind::Symlink);
                assert_eq!(&contents(&mut volume, got), target, "{}", entry.path);
            }
            What::Directory => assert_eq!(got.kind, Kind::Directory),
        }
    }
    assert_eq!(by_path["/share/nested/deep"].mode, 0o040755);
    let mut middle = [0; 10];
    let lib = volume.inode(by_path["/lib/libx.so"].ino).unwrap();
    assert_eq!(
        volume
            .read(&lib, 2 * BLOCK as u64 - 5, &mut middle)
            .unwrap(),
        10
    );
    assert_eq!(&middle[..], &tree[1].1[2 * BLOCK - 5..2 * BLOCK + 5]);
}

#[test]
fn a_tree_lays_out_the_same_every_time() {
    assert!(build(&sample()) == build(&sample()));
    let mut reversed = sample();
    reversed.reverse();
    assert!(build(&sample()) == build(&reversed));
}

#[test]
fn a_tree_that_names_something_twice_or_under_a_file_is_refused() {
    let entries =
        |paths: &[&str]| -> Vec<Entry> { paths.iter().map(|p| file(p, 0o644, b"x").0).collect() };
    for bad in [
        &["/a", "/a"][..],
        &["/a", "/a/b"],
        &["a"],
        &["/a/../b"],
        &["/a//b"],
        &["/lost+found/x"],
        &["/"],
    ] {
        assert!(layout(&entries(bad), UUID).is_err(), "{bad:?}");
    }
    let empty = symlink("/link", "").0;
    assert!(layout(&[empty], UUID).is_err());
}

/// The block and byte a directory entry named `name` starts at.
fn dirent_of(image: &[u8], name: &[u8]) -> usize {
    let needle: Vec<u8> = [&[name.len() as u8][..], &[0u8][..], name].concat();
    let at = image
        .windows(needle.len())
        .position(|w| w[0] == needle[0] && w[2..] == needle[2..])
        .unwrap();
    at - 6
}

fn reseal_dir(image: &mut [u8], at: usize, ino: u32) {
    let block = at / BLOCK * BLOCK;
    let seed = slopos_ext4_core::crc::crc32c(!0, &UUID);
    slopos_ext4_core::dir::seal(
        slopos_ext4_core::inode::seed(seed, ino, 0),
        &mut image[block..block + BLOCK],
    );
}

#[test]
fn a_directory_that_holds_a_name_twice_or_lies_about_its_parent_is_refused() {
    let tree = vec![file("/bin/aa", 0o644, b"a"), file("/bin/bb", 0o644, b"b")];
    let mut image = build(&tree);
    let at = dirent_of(&image, b"bb");
    image[at + 8..at + 10].copy_from_slice(b"aa");
    reseal_dir(&mut image, at, 12);
    let blocks = (image.len() / BLOCK) as u64;
    let mut volume = Volume::open(Memory(image), blocks).unwrap();
    assert!(matches!(
        volume.walk(100, 1 << 24),
        Err(VolumeError::Damaged(_, 12))
    ));

    let mut image = build(&tree);
    let dot_dot = dirent_of(&image, b"aa") / BLOCK * BLOCK + 12;
    image[dot_dot..dot_dot + 4].copy_from_slice(&12u32.to_le_bytes());
    reseal_dir(&mut image, dot_dot, 12);
    let mut volume = Volume::open(Memory(image), blocks).unwrap();
    assert!(volume.walk(100, 1 << 24).is_err());
}

/// Where inode `ino` lies, by group 0's descriptor.
fn inode_at(image: &[u8], ino: u32) -> usize {
    let word = |at: usize| u64::from(u32::from_le_bytes(image[at..at + 4].try_into().unwrap()));
    let table = word(BLOCK + 0x08) | word(BLOCK + 0x28) << 32;
    table as usize * BLOCK + (ino as usize - 1) * 256
}

fn reseal_inode(image: &mut [u8], ino: u32) {
    let at = inode_at(image, ino);
    let seed = slopos_ext4_core::crc::crc32c(!0, &UUID);
    slopos_ext4_core::inode::seal(seed, ino, &mut image[at..at + 256]);
}

/// A file whose extent names a block before the volume's own or runs past
/// its end, or whose inode the writer never numbers that generation, is
/// refused when read, as the kernel refuses to serve it.
#[test]
fn a_file_the_kernel_would_not_serve_is_refused() {
    let image = build(&sample());
    let blocks = (image.len() / BLOCK) as u64;
    let ino = {
        let mut volume = Volume::open(Memory(image.clone()), blocks).unwrap();
        let walked = volume.walk(100, 1 << 24).unwrap();
        walked.iter().find(|f| f.path == "/share/one").unwrap().ino
    };
    let extent = inode_at(&image, ino) + slopos_ext4_core::inode::off::BLOCK + 12;
    let read = |image: Vec<u8>| {
        let mut volume = Volume::open(Memory(image), blocks)?;
        let inode = volume.inode(ino)?;
        volume.read_all(&inode, u64::MAX)
    };
    assert!(read(image.clone()).is_ok());
    let mut at_zero = image.clone();
    at_zero[extent + 8..extent + 12].copy_from_slice(&0u32.to_le_bytes());
    reseal_inode(&mut at_zero, ino);
    assert!(matches!(read(at_zero), Err(VolumeError::Damaged(_, _))));
    let mut past = image.clone();
    past[extent + 4..extent + 6].copy_from_slice(&u16::try_from(blocks).unwrap().to_le_bytes());
    reseal_inode(&mut past, ino);
    assert!(matches!(read(past), Err(VolumeError::Damaged(_, _))));
    let mut other = image.clone();
    let generation = inode_at(&other, ino) + slopos_ext4_core::inode::off::GENERATION;
    other[generation] = 1;
    reseal_inode(&mut other, ino);
    assert!(matches!(read(other), Err(VolumeError::Unsupported(_))));
    let mut endless = image;
    let at = inode_at(&endless, ino);
    for field in [
        slopos_ext4_core::inode::off::SIZE_LO,
        slopos_ext4_core::inode::off::SIZE_HIGH,
    ] {
        endless[at + field..at + field + 4].copy_from_slice(&[0xff; 4]);
    }
    reseal_inode(&mut endless, ino);
    let mut volume = Volume::open(Memory(endless), blocks).unwrap();
    assert!(matches!(
        volume.walk(100, 1 << 24),
        Err(VolumeError::Damaged("files larger than the volume", _))
    ));
}

#[test]
fn a_volume_of_other_geometry_or_features_is_refused() {
    let image = build(&sample());
    let blocks = (image.len() / BLOCK) as u64;
    assert!(matches!(
        Volume::open(Memory(image.clone()), blocks + 1),
        Err(VolumeError::Damaged(..))
    ));
    let mut writable = image.clone();
    let ro = 1024 + slopos_ext4_core::superblock::off::FEATURE_RO_COMPAT;
    writable[ro + 1] &= !0x10;
    slopos_ext4_core::superblock::seal(&mut writable[1024..2048]);
    assert!(matches!(
        Volume::open(Memory(writable), blocks),
        Err(VolumeError::Unsupported(_))
    ));
    let mut flipped = image.clone();
    flipped[1024 + 0x40] ^= 1;
    assert!(matches!(
        Volume::open(Memory(flipped), blocks),
        Err(VolumeError::Damaged(..))
    ));
    let mut unsummed = image.clone();
    unsummed[ro + 1] &= !0x04;
    slopos_ext4_core::superblock::seal(&mut unsummed[1024..2048]);
    assert!(matches!(
        Volume::open(Memory(unsummed), blocks),
        Err(VolumeError::Unsupported(_))
    ));
    let desc_size = 1024 + slopos_ext4_core::superblock::off::DESC_SIZE;
    for size in [16u16, 32, 128, 5000] {
        let mut resized = image.clone();
        resized[desc_size..desc_size + 2].copy_from_slice(&size.to_le_bytes());
        slopos_ext4_core::superblock::seal(&mut resized[1024..2048]);
        assert!(matches!(
            Volume::open(Memory(resized), blocks),
            Err(VolumeError::Damaged(..))
        ));
    }
}

#[test]
fn a_walk_stops_at_its_limit() {
    let image = build(&sample());
    let blocks = (image.len() / BLOCK) as u64;
    let mut volume = Volume::open(Memory(image), blocks).unwrap();
    assert_eq!(volume.walk(5, 1 << 24), Err(VolumeError::TooMany));
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-V").output().is_ok()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("slopos-volume-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write the volume `entries` lay out to `path`, sparse where a block is
/// zero, each file's bytes from `fill`.
fn write_file(entries: &[Entry], path: &Path, fill: impl Fn(usize, u64) -> u8) -> u64 {
    let layout = layout(entries, UUID).unwrap();
    let mut out = std::fs::File::create(path).unwrap();
    layout
        .write::<std::io::Error>(
            &mut |i, offset, room| {
                for (k, b) in room.iter_mut().enumerate() {
                    *b = fill(i, offset + k as u64);
                }
                Ok(())
            },
            &mut |block| {
                if block.iter().all(|&b| b == 0) {
                    out.seek(SeekFrom::Current(BLOCK as i64)).map(drop)
                } else {
                    out.write_all(block)
                }
            },
        )
        .unwrap();
    out.set_len(layout.blocks() * BLOCK as u64).unwrap();
    layout.blocks()
}

fn e2fsck_passes(path: &Path) {
    let out = Command::new("e2fsck")
        .arg("-fn")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "e2fsck -fn {}:\n{}{}",
        path.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn e2fsck_holds_a_volume_of_one_group_and_of_many() {
    if !have("e2fsck") {
        std::eprintln!("e2fsck is not installed; skipping");
        return;
    }
    let dir = scratch("e2fsck");
    let small: Vec<Entry> = sample().into_iter().map(|(e, _)| e).collect();
    write_file(&small, &dir.join("small.img"), |_, at| at as u8 | 1);
    e2fsck_passes(&dir.join("small.img"));

    let big = 700u64 << 20;
    let mut many = small.clone();
    many.push(file("/share/big", 0o644, b"").0);
    if let Some(last) = many.last_mut() {
        last.what = What::File(big);
    }
    let blocks = write_file(&many, &dir.join("many.img"), |i, at| {
        if i + 1 == many.len() && at % (1 << 20) != 0 {
            0
        } else {
            1
        }
    });
    assert!(blocks > 5 * 32768, "{blocks} blocks span groups 1 to 5");
    e2fsck_passes(&dir.join("many.img"));
    let mut volume = Volume::open(
        File(std::fs::File::open(dir.join("many.img")).unwrap()),
        blocks,
    )
    .unwrap();
    let found = volume.walk(100, 1 << 24).unwrap();
    let big_file = found.iter().find(|f| f.path == "/share/big").unwrap();
    let inode = volume.inode(big_file.ino).unwrap();
    for at in [0, 1 << 20, (300 << 20) + 1, big - 1] {
        let mut byte = [9];
        assert_eq!(volume.read(&inode, at, &mut byte).unwrap(), 1);
        assert_eq!(byte[0], u8::from(at % (1 << 20) == 0), "{at}");
    }

    let names: Vec<Entry> = (0..40_000)
        .map(|i| file(&format!("/share/n{i:05}"), 0o644, b"").0)
        .collect();
    let blocks = write_file(&names, &dir.join("names.img"), |_, _| 0);
    e2fsck_passes(&dir.join("names.img"));
    let mut volume = Volume::open(
        File(std::fs::File::open(dir.join("names.img")).unwrap()),
        blocks,
    )
    .unwrap();
    assert_eq!(volume.walk(50_000, 1 << 24).unwrap().len(), 40_000 + 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_volume_mke2fs_made_reads_as_its_tree() {
    if !have("mke2fs") || !have("tune2fs") {
        std::eprintln!("mke2fs is not installed; skipping");
        return;
    }
    let dir = scratch("mke2fs");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("bin")).unwrap();
    std::fs::create_dir_all(tree.join("share/doc")).unwrap();
    std::fs::write(tree.join("bin/tool"), b"tool bytes").unwrap();
    let big: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(tree.join("share/doc/big"), &big).unwrap();
    std::os::unix::fs::symlink("../bin/tool", tree.join("share/link")).unwrap();
    let image = dir.join("made.img");
    let profile = include_str!("../../../ext4-core/profile");
    let features = profile
        .lines()
        .find_map(|l| l.strip_prefix("features="))
        .unwrap()
        .replace("has_journal,", "");
    let made = Command::new("mke2fs")
        .args([
            "-q", "-F", "-t", "ext4", "-O", "none", "-O", &features, "-b", "4096", "-I", "256",
            "-d",
        ])
        .arg(&tree)
        .arg(&image)
        .arg("4M")
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let tuned = Command::new("tune2fs")
        .args(["-O", "read-only"])
        .arg(&image)
        .output()
        .unwrap();
    assert!(tuned.status.success(), "{tuned:?}");
    let blocks = std::fs::metadata(&image).unwrap().len() / BLOCK as u64;
    let mut volume = Volume::open(File(std::fs::File::open(&image).unwrap()), blocks).unwrap();
    let found = volume.walk(100, 1 << 24).unwrap();
    let get = |path: &str| found.iter().find(|f| f.path == path).cloned().unwrap();
    let read = |volume: &mut Volume<File>, path: &str| {
        let inode = volume.inode(get(path).ino).unwrap();
        volume.read_all(&inode, u64::MAX).unwrap()
    };
    assert_eq!(read(&mut volume, "/bin/tool"), b"tool bytes");
    assert_eq!(read(&mut volume, "/share/doc/big"), big);
    assert_eq!(read(&mut volume, "/share/link"), b"../bin/tool");
    assert_eq!(get("/lost+found").kind, Kind::Directory);
    let _ = std::fs::remove_dir_all(&dir);
}
