use std::path::PathBuf;
use std::process::Command;
use std::string::{String, ToString};
use std::vec::Vec;
use std::{format, fs, vec};

use super::*;

/// A volume in memory, recording every write and flush so a test can replay
/// any prefix of them as the medium a crash would leave.
#[derive(Clone)]
struct MemDevice {
    bytes: Vec<u8>,
    log: Vec<Op>,
    block: u32,
}

#[derive(Clone)]
enum Op {
    Write(u64, Vec<u8>),
    Flush,
}

impl MemDevice {
    fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
            log: Vec::new(),
            block: 512,
        }
    }
}

impl Device for MemDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let at = offset as usize;
        let src = self.bytes.get(at..at + buf.len()).ok_or(Error::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Error> {
        let at = offset as usize;
        self.bytes
            .get_mut(at..at + buf.len())
            .ok_or(Error::Io)?
            .copy_from_slice(buf);
        self.log.push(Op::Write(offset, buf.to_vec()));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.log.push(Op::Flush);
        Ok(())
    }

    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn block_size(&self) -> u32 {
        self.block
    }
}

const MIB: usize = 1024 * 1024;
const LABEL: &[u8; 11] = b"SLOPOS ESP ";
const SERIAL: u32 = 0x5105_0505;

fn fresh(size: usize) -> Volume<MemDevice> {
    let mut dev = MemDevice::new(size);
    format(&mut dev, size as u64, LABEL, SERIAL).expect("format");
    dev.log.clear();
    Volume::open(dev).expect("open")
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A 4K-native disk formats with 4096-byte sectors, and a volume laid out
/// in 512-byte sectors is refused there: none of its sectors is a whole block.
#[test]
fn sectors_follow_the_device_block_size() {
    let size = 272 * MIB;
    let mut dev = MemDevice::new(size);
    dev.block = 4096;
    format(&mut dev, size as u64, LABEL, SERIAL).expect("format on 4K blocks");
    assert_eq!(u16::from_le_bytes([dev.bytes[11], dev.bytes[12]]), 4096);
    let mut volume = Volume::open(dev).expect("open on 4K blocks");
    let data = pattern(10_000, 3);
    volume.write_file("/K.BIN", &data).unwrap();
    assert_eq!(volume.read_file("/K.BIN").unwrap(), data);

    let mut small = MemDevice::new(64 * MIB);
    format(&mut small, 64 * MIB as u64, LABEL, SERIAL).unwrap();
    small.block = 4096;
    assert_eq!(Volume::open(small).err(), Some(Error::SectorTooSmall));
}

#[test]
fn files_and_directories_round_trip() {
    let mut v = fresh(64 * MIB);
    let before = v.free_clusters();
    v.create_dir("/boot").unwrap();
    v.create_dir("/boot/a").unwrap();
    let kernel = pattern(3 * v.cluster_bytes() + 17, 7);
    v.write_file("/boot/a/kernel.elf", &kernel).unwrap();
    v.write_file("/boot/a/empty", &[]).unwrap();
    assert_eq!(v.read_file("/boot/a/kernel.elf").unwrap(), kernel);
    assert_eq!(v.read_file("/BOOT/A/KERNEL.ELF").unwrap(), kernel);
    assert!(v.read_file("/boot/a/empty").unwrap().is_empty());

    let names: Vec<String> = v
        .list("/boot/a")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["kernel.elf", "empty"]);
    assert_eq!(v.stat("/boot").unwrap().is_dir, true);
    assert_eq!(v.create_dir("/boot/a"), Err(Error::Exists));
    assert_eq!(v.remove("/boot/a"), Err(Error::DirectoryNotEmpty));

    v.remove("/boot/a/kernel.elf").unwrap();
    v.remove("/boot/a/empty").unwrap();
    v.remove("/boot/a").unwrap();
    v.remove("/boot").unwrap();
    assert_eq!(v.stat("/boot"), Err(Error::NotFound));
    assert_eq!(v.free_clusters(), before);

    // Everything above survives a fresh mount of the same bytes.
    let mut again = Volume::open(v.into_device()).unwrap();
    assert!(again.list("/").unwrap().is_empty());
}

#[test]
fn replacing_a_file_frees_its_old_chain() {
    let mut v = fresh(64 * MIB);
    let before = v.free_clusters();
    let big = pattern(10 * v.cluster_bytes(), 1);
    let small = pattern(100, 2);
    v.write_file("/cfg", &big).unwrap();
    v.write_file("/cfg", &small).unwrap();
    assert_eq!(v.read_file("/cfg").unwrap(), small);
    assert_eq!(v.free_clusters(), before - 1);
    let mut again = Volume::open(v.into_device()).unwrap();
    assert_eq!(again.read_file("/cfg").unwrap(), small);
}

#[test]
fn a_file_in_holes_between_live_ones_reads_back() {
    let mut v = fresh(64 * MIB);
    let cb = v.cluster_bytes();
    for (i, name) in ["/h0", "/k0", "/h1", "/k1"].iter().enumerate() {
        v.write_file(name, &pattern(3 * cb, i as u8)).unwrap();
    }
    v.remove("/h0").unwrap();
    v.remove("/h1").unwrap();
    // Two three-cluster holes, then the free tail: at least three runs, the
    // tail one past MAX_EXTENT.
    let big = pattern(2 * MIB + 5 * cb + 3, 9);
    v.write_file("/big", &big).unwrap();
    assert_eq!(v.read_file("/big").unwrap(), big);
    assert_eq!(v.read_file("/k0").unwrap(), pattern(3 * cb, 1));
    assert_eq!(v.read_file("/k1").unwrap(), pattern(3 * cb, 3));
    let mut again = Volume::open(v.into_device()).unwrap();
    assert_eq!(again.read_file("/big").unwrap(), big);
}

#[test]
fn a_full_volume_refuses_and_leaks_nothing() {
    let mut v = fresh(40 * MIB);
    let free = v.free_clusters();
    let too_big = vec![0xA5u8; (free as usize + 1) * v.cluster_bytes()];
    assert_eq!(v.write_file("/big", &too_big), Err(Error::NoSpace));
    assert_eq!(v.free_clusters(), free);
    assert_eq!(v.stat("/big"), Err(Error::NotFound));
}

#[test]
fn a_directory_grows_past_one_cluster() {
    let mut v = fresh(64 * MIB);
    v.create_dir("/d").unwrap();
    let per_cluster = v.cluster_bytes() / ENTRY;
    let count = per_cluster * 2 + 3;
    for i in 0..count {
        v.write_file(&format!("/d/f{i}"), &[i as u8]).unwrap();
    }
    let mut again = Volume::open(v.into_device()).unwrap();
    let listed = again.list("/d").unwrap();
    assert_eq!(listed.len(), count);
    for i in 0..count {
        assert_eq!(again.read_file(&format!("/d/f{i}")).unwrap(), [i as u8]);
    }
}

/// The data region starts on a MiB of the volume, whatever the sector size,
/// and the volume still reads back.
#[test]
fn the_data_region_starts_on_a_mebibyte() {
    for (block, size) in [(512u32, 272 * MIB), (4096, 272 * MIB), (512, 1024 * MIB)] {
        let mut dev = MemDevice::new(size);
        dev.block = block;
        format(&mut dev, size as u64, LABEL, SERIAL).unwrap();
        let reserved = u64::from(le16(&dev.bytes, 14));
        let fat = u64::from(le32(&dev.bytes, 36));
        assert_eq!(
            (reserved + 2 * fat) * u64::from(block) % (1 << 20),
            0,
            "{block} {size}"
        );
        let mut v = Volume::open(dev).unwrap();
        v.write_file("/K.BIN", b"kernel").unwrap();
        assert_eq!(v.read_file("/K.BIN").unwrap(), b"kernel");
    }
}

#[test]
fn eight_dot_three_names_keep_their_case_in_ntres() {
    assert!(short_name("KERNEL.ELF").is_ok());
    assert_eq!(
        short_name("kernel.elf").unwrap().1,
        NTRES_LOWER_BASE | NTRES_LOWER_EXT
    );
    assert_eq!(&short_name("a").unwrap().0, b"A          ");
    for long in [
        "",
        "limine.conf",
        "ninechars",
        "Mixed.elf",
        "a b",
        "x.",
        ".x",
        "a.b.c",
    ] {
        assert_eq!(
            short_name(long).map(|_| ()),
            Err(Error::InvalidName),
            "{long}"
        );
    }
}

fn shorts(v: &mut Volume<MemDevice>, dir: &str) -> Vec<(String, String)> {
    v.list(dir)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.short_name))
        .collect()
}

#[test]
fn long_names_round_trip_with_generated_aliases() {
    let mut v = fresh(64 * MIB);
    v.create_dir("/EFI").unwrap();
    v.create_dir("/EFI/SlopOS").unwrap();
    v.write_file("/EFI/SlopOS/limine.conf", b"timeout: 5\n")
        .unwrap();
    v.write_file("/EFI/SlopOS/LICENSE.limine", b"BSD").unwrap();
    let long = "a name long enough to take three long entries.txt";
    v.write_file(&format!("/EFI/{long}"), b"x").unwrap();
    v.write_file("/EFI/Ünïcødé.bin", b"u").unwrap();
    let mut again = Volume::open(v.into_device()).unwrap();
    assert_eq!(
        shorts(&mut again, "/EFI/SlopOS"),
        [
            ("limine.conf".to_string(), "LIMINE~1.CON".to_string()),
            ("LICENSE.limine".to_string(), "LICENS~1.LIM".to_string()),
        ]
    );
    assert_eq!(
        shorts(&mut again, "/EFI"),
        [
            ("SlopOS".to_string(), "SLOPOS".to_string()),
            (long.to_string(), "ANAMEL~1.TXT".to_string()),
            ("Ünïcødé.bin".to_string(), "_N_C_D~1.BIN".to_string()),
        ]
    );
    assert_eq!(
        again.read_file("/efi/slopos/LIMINE.CONF").unwrap(),
        b"timeout: 5\n"
    );
    assert_eq!(
        again.read_file("/EFI/SLOPOS/LIMINE~1.CON").unwrap(),
        b"timeout: 5\n"
    );
    assert_eq!(
        again.write_file("/EFI/slopos", b"x"),
        Err(Error::IsADirectory),
        "a long name matches without case"
    );
}

#[test]
fn an_alias_takes_the_lowest_tail_no_entry_has() {
    let mut v = fresh(64 * MIB);
    for name in ["limine.conf", "limine.config", "Limine.Cfg"] {
        v.write_file(&format!("/{name}"), name.as_bytes()).unwrap();
    }
    let got: Vec<String> = shorts(&mut v, "/").into_iter().map(|(_, s)| s).collect();
    assert_eq!(got, ["LIMINE~1.CON", "LIMINE~2.CON", "LIMINE.CFG"]);
    v.write_file("/Mixed.Elf", b"x").unwrap();
    v.write_file("/MIXED.elf", b"y").unwrap();
    assert_eq!(
        v.read_file("/mixed.ELF").unwrap(),
        b"y",
        "one file, any case"
    );
    let taken: Vec<[u8; 11]> = (1..=9)
        .map(|n| {
            let mut short = *b"LONGNA~0TXT";
            short[7] = b'0' + n;
            short
        })
        .collect();
    assert_eq!(&alias("longname file.txt", &taken).unwrap(), b"LONGN~10TXT");
}

#[test]
fn a_name_a_long_entry_cannot_hold_is_refused() {
    let mut v = fresh(64 * MIB);
    let too_long = "x".repeat(256);
    for bad in ["", "a:b", "a*b", "x.", "trailing ", "tab\there", &too_long] {
        assert_eq!(
            v.write_file(&format!("/{bad}"), b"x"),
            Err(Error::InvalidName),
            "{bad:?}"
        );
    }
    v.write_file(&format!("/{}", "y".repeat(255)), b"y")
        .unwrap();
}

#[test]
fn removing_a_long_named_file_frees_every_entry_it_took() {
    let mut v = fresh(64 * MIB);
    let name = "/a name that takes two long entries";
    v.write_file(name, b"1").unwrap();
    v.write_file("/after", b"2").unwrap();
    v.remove(name).unwrap();
    assert!(v.stat(name).is_err());
    v.write_file(name, b"3").unwrap();
    let mut again = Volume::open(v.into_device()).unwrap();
    assert_eq!(again.read_file(name).unwrap(), b"3");
    assert_eq!(again.read_file("/after").unwrap(), b"2");
    assert_eq!(again.list("/").unwrap().len(), 2);
}

/// A run of entries reaches into a cluster the directory gains for it.
#[test]
fn long_entries_span_a_directory_cluster_boundary() {
    let mut v = fresh(64 * MIB);
    v.create_dir("/d").unwrap();
    let per_cluster = v.cluster_bytes() / ENTRY;
    for i in 0..per_cluster - 3 {
        v.write_file(&format!("/d/f{i}"), &[i as u8]).unwrap();
    }
    let long = "a name long enough to take three long entries.txt";
    v.write_file(&format!("/d/{long}"), b"spans").unwrap();
    let mut again = Volume::open(v.into_device()).unwrap();
    assert_eq!(again.read_file(&format!("/d/{long}")).unwrap(), b"spans");
    assert_eq!(again.list("/d").unwrap().len(), per_cluster - 3 + 1);
}

/// A crash at any point of creating a long-named file shows it under its
/// long name or not at all, never as its alias alone.
#[test]
fn a_long_named_file_appears_whole_or_not_at_all() {
    let v = fresh(64 * MIB);
    let base = v.into_device();
    let mut v = Volume::open(base.clone()).unwrap();
    v.write_file("/limine.conf", b"timeout: 5\n").unwrap();
    let log = v.into_device().log;
    for cut in 0..=log.len() {
        let mut image = base.clone();
        for op in &log[..cut] {
            if let Op::Write(at, data) = op {
                image.bytes[*at as usize..*at as usize + data.len()].copy_from_slice(data);
            }
        }
        let names: Vec<String> = Volume::open(image)
            .unwrap()
            .list("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            names.is_empty() || names == ["limine.conf"],
            "cut {cut}: {names:?}"
        );
    }
}

/// A format cut short leaves no boot sector, so the partition reads as
/// holding no volume rather than a damaged one: the boot sector is written
/// last, behind everything it describes.
#[test]
fn a_volume_starts_only_when_its_format_is_whole() {
    let mut dev = MemDevice::new(64 * MIB);
    dev.bytes.fill(0xA5);
    dev.bytes[..512].fill(0);
    let stale = dev.clone();
    format(&mut dev, 64 * MIB as u64, LABEL, SERIAL).unwrap();
    let log = dev.log;
    let boot_at = log
        .iter()
        .rposition(|op| matches!(op, Op::Write(0, data) if data[510..512] == [0x55, 0xAA]))
        .expect("the boot sector is written");
    assert!(matches!(log[boot_at - 1], Op::Flush));
    assert_eq!(boot_at, log.len() - 2);
    for cut in 0..=log.len() {
        let mut image = stale.clone();
        for op in &log[..cut] {
            if let Op::Write(at, data) = op {
                image.bytes[*at as usize..*at as usize + data.len()].copy_from_slice(data);
            }
        }
        let signed = image.bytes[510..512] == [0x55, 0xAA];
        assert_eq!(signed, cut > boot_at, "cut {cut}");
        if signed {
            assert!(Volume::open(image).unwrap().list("/").unwrap().is_empty());
        }
    }
}

/// Replay every prefix of the writes a replacement issued: the file must read
/// as wholly old or wholly new at each, never a mix and never missing.
#[test]
fn a_replacement_is_never_half_visible() {
    let mut v = fresh(64 * MIB);
    let old = pattern(5 * v.cluster_bytes() + 9, 3);
    let new = pattern(7 * v.cluster_bytes() + 1, 4);
    v.write_file("/kernel", &old).unwrap();
    let base = v.into_device();
    let mut dev = base.clone();
    dev.log.clear();
    let mut v = Volume::open(dev).unwrap();
    v.write_file("/kernel", &new).unwrap();
    let log = v.into_device().log;

    let mut saw_new = false;
    for cut in 0..=log.len() {
        let mut image = base.clone();
        for op in &log[..cut] {
            if let Op::Write(at, data) = op {
                image.bytes[*at as usize..*at as usize + data.len()].copy_from_slice(data);
            }
        }
        let mut crashed = Volume::open(image).unwrap();
        let read = crashed.read_file("/kernel").unwrap();
        assert!(read == old || read == new, "cut {cut} read a mix");
        saw_new |= read == new;
    }
    assert!(saw_new);

    // The commit is one write, and a flush orders everything it depends on
    // ahead of it.
    let commit = log
        .iter()
        .rposition(|op| matches!(op, Op::Write(_, d) if d.len() == ENTRY))
        .expect("the directory entry store");
    assert!(matches!(log[commit - 1], Op::Flush));
}

fn host_tool(name: &str) -> bool {
    Command::new(name).arg("--help").output().is_ok()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("slopos-fat-core-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

struct FileDevice(fs::File, u64);

impl Device for FileDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        use std::os::unix::fs::FileExt;
        self.0.read_exact_at(buf, offset).map_err(|_| Error::Io)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Error> {
        use std::os::unix::fs::FileExt;
        self.0.write_all_at(buf, offset).map_err(|_| Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.1
    }
}

fn open_file(path: &PathBuf) -> Volume<FileDevice> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let len = file.metadata().unwrap().len();
    Volume::open(FileDevice(file, len)).unwrap()
}

fn fsck_clean(path: &PathBuf) {
    let out = Command::new("fsck.fat")
        .arg("-n")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fsck.fat: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A volume `mkfs.fat` made and `mcopy` filled — long names included — is
/// read and updated here, and `fsck.fat` and `mtools` agree with the result.
#[test]
fn interoperates_with_dosfstools_and_mtools() {
    if !(host_tool("mkfs.fat") && host_tool("fsck.fat") && host_tool("mcopy")) {
        std::eprintln!("skipped: needs mkfs.fat, fsck.fat and mtools");
        return;
    }
    let img = scratch("esp.img");
    let _ = fs::remove_file(&img);
    let ok = Command::new("mkfs.fat")
        .args(["-F", "32", "-C"])
        .arg(&img)
        .arg((96 * 1024).to_string())
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let conf = scratch("limine.conf");
    fs::write(&conf, b"timeout: 0\n").unwrap();
    let ok = Command::new("mcopy")
        .arg("-i")
        .arg(&img)
        .arg(&conf)
        .arg("::/limine.conf")
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );

    let mut v = open_file(&img);
    assert_eq!(v.read_file("/limine.conf").unwrap(), b"timeout: 0\n");
    v.write_file("/limine.conf", b"timeout: 5\ndefault_entry: 2\n")
        .unwrap();
    v.create_dir("/boot").unwrap();
    let kernel = pattern(300_000, 9);
    v.write_file("/boot/kernel.elf", &kernel).unwrap();
    drop(v);
    fsck_clean(&img);

    let out = Command::new("mtype")
        .arg("-i")
        .arg(&img)
        .arg("::/limine.conf")
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"timeout: 5\ndefault_entry: 2\n");
    let back = scratch("kernel.back");
    let ok = Command::new("mcopy")
        .arg("-n")
        .arg("-i")
        .arg(&img)
        .arg("::/boot/kernel.elf")
        .arg(&back)
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(fs::read(&back).unwrap(), kernel);

    // And what `format` lays down is a volume `fsck.fat` accepts.
    let ours = scratch("ours.img");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&ours)
        .unwrap();
    file.set_len(64 * MIB as u64).unwrap();
    let mut dev = FileDevice(file, 64 * MIB as u64);
    format(&mut dev, 64 * MIB as u64, LABEL, SERIAL).unwrap();
    let mut v = Volume::open(dev).unwrap();
    v.create_dir("/efi").unwrap();
    v.write_file("/efi/x.bin", &pattern(5000, 1)).unwrap();
    v.create_dir("/EFI/SlopOS").unwrap();
    v.write_file("/EFI/SlopOS/limine.conf", b"timeout: 5\n")
        .unwrap();
    drop(v);
    fsck_clean(&ours);
    let out = Command::new("mtype")
        .arg("-i")
        .arg(&ours)
        .arg("::/EFI/SlopOS/limine.conf")
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"timeout: 5\n");
    let out = Command::new("mdir")
        .args(["-b", "-i"])
        .arg(&ours)
        .arg("::/EFI")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("::/EFI/SlopOS/"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(img.parent().unwrap());
}
