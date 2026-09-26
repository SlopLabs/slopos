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
}

const MIB: usize = 1024 * 1024;
const LABEL: &[u8; 11] = b"SLOPOS ESP ";

fn fresh(size: usize) -> Volume<MemDevice> {
    let mut dev = MemDevice::new(size);
    format(&mut dev, size as u64, LABEL).expect("format");
    dev.log.clear();
    Volume::open(dev).expect("open")
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
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

#[test]
fn created_names_must_fit_eight_dot_three() {
    assert!(short_name("KERNEL.ELF").is_ok());
    assert_eq!(
        short_name("kernel.elf").unwrap().1,
        NTRES_LOWER_BASE | NTRES_LOWER_EXT
    );
    assert_eq!(&short_name("a").unwrap().0, b"A          ");
    for bad in [
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
            short_name(bad).map(|_| ()),
            Err(Error::InvalidName),
            "{bad}"
        );
    }
    let mut v = fresh(64 * MIB);
    assert_eq!(v.write_file("/limine.conf", b"x"), Err(Error::InvalidName));
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
    format(&mut dev, 64 * MIB as u64, LABEL).unwrap();
    let mut v = Volume::open(dev).unwrap();
    v.create_dir("/efi").unwrap();
    v.write_file("/efi/x.bin", &pattern(5000, 1)).unwrap();
    drop(v);
    fsck_clean(&ours);
    let _ = fs::remove_dir_all(img.parent().unwrap());
}
