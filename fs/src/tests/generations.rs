//! Base generations: an archive copied in and served from its own store, an
//! inode that names its generation for good, residency while something holds
//! one, every mount of the base moved in one step, and an archive the kernel
//! cannot serve refused. Each test owns a `BaseFs` of its own: the boot's is
//! installed after this phase.

use slopos_abi::Errno;
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFREG};
use slopos_boot_core::base::INTERFACE_PATH;
use slopos_boot_core::newc::Record;
use slopos_ostd::KVec;
use slopos_ostd::process::{Program, process_retire, process_spawn, root_account};
use slopos_testing::TestResult;

use super::BASE_SAMPLE;
use crate::basefs::activate::{ActivateError, activate_on};
use crate::basefs::reap::runs_from;
use crate::basefs::{BASE_FS, BaseFs, StageError, generation_of, stage};
use crate::vfs::mount::{mount_at, mount_subtree, unmount};
use crate::vfs::traits::{FileSystem, FileType, InodeId};
use crate::vfs::{VfsError, vfs_init_builtin_filesystems, vfs_mkdir};

const DIR: u32 = S_IFDIR | 0o755;
const FILE: u32 = S_IFREG | 0o644;
const BIG: usize = 70_000;

pub fn archive(records: &[(&str, u32, &[u8])]) -> Option<KVec<u8>> {
    let mut out = KVec::new();
    for &(path, mode, data) in records {
        append(&mut out, &Record::new(path.as_bytes(), mode, data)?)?;
    }
    append(&mut out, &Record::trailer())?;
    Some(out)
}

fn append(out: &mut KVec<u8>, record: &Record<'_>) -> Option<()> {
    for piece in record.pieces() {
        out.extend_from_slice(piece).ok()?;
    }
    Some(())
}

/// `n` as a line of decimal digits.
fn decimal(n: u32, buf: &mut [u8; 12]) -> &[u8] {
    let mut n = n;
    let mut at = buf.len() - 1;
    buf[at] = b'\n';
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[at..];
        }
    }
}

/// The interface a served archive names.
fn interface(buf: &mut [u8; 12]) -> &[u8] {
    decimal(slopos_abi::BASE_INTERFACE, buf)
}

/// `archive` handed over at most `piece` bytes a read.
pub fn pieces(
    archive: &[u8],
    piece: usize,
) -> impl FnMut(u64, &mut [u8]) -> Result<usize, Errno> + '_ {
    move |offset, buf| {
        let start = usize::try_from(offset).map_or(archive.len(), |o| o.min(archive.len()));
        let n = (archive.len() - start).min(buf.len()).min(piece);
        buf[..n].copy_from_slice(&archive[start..start + n]);
        Ok(n)
    }
}

fn activate(fs: &'static BaseFs, image: &[u8]) -> Result<u32, ActivateError> {
    activate_on(fs, &[], &mut pieces(image, 4093), image.len() as u64)
}

pub(super) fn read_all(fs: &BaseFs, inode: InodeId, buf: &mut [u8]) -> Option<usize> {
    let mut done = 0;
    loop {
        match fs.read(inode, done as u64, &mut buf[done..]) {
            Ok(0) => return Some(done),
            Ok(n) => done += n,
            Err(_) => return None,
        }
    }
}

fn pattern(i: usize) -> u8 {
    (i % 251) as u8
}

static STAGED: BaseFs = BaseFs::new();

/// An archive read a few kilobytes at a time is copied into the generation's
/// own store and served from it: a file across the store's pieces read back
/// at any offset, a symlink's target up to its NUL, directories the archive
/// only implies.
pub fn test_a_staged_generation_serves_its_copy() -> TestResult {
    let mut big = KVec::new();
    if big.resize(BIG, 0).is_err() {
        return slopos_testing::fail!("no room for the fixture");
    }
    for (i, byte) in big.iter_mut().enumerate() {
        *byte = pattern(i);
    }
    let mut text = [0u8; 12];
    let Some(image) = archive(&[
        ("/usr/share/slopos", DIR, b""),
        (INTERFACE_PATH, FILE, interface(&mut text)),
        ("/bin/big", S_IFREG | 0o755, &big),
        ("/bin/sh", S_IFLNK | 0o777, b"big\0junk"),
    ]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if STAGED.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("the first generation did not index");
    }
    let number = match activate(&STAGED, &image) {
        Ok(number) => number,
        Err(e) => return slopos_testing::fail!("the archive did not activate: {:?}", e),
    };
    if number != 2 || STAGED.current_generation() != 2 {
        return slopos_testing::fail!("the new generation is not number 2 and current");
    }
    match serves_its_copy() {
        Ok(()) => TestResult::Pass,
        Err(why) => slopos_testing::fail!("{}", why),
    }
}

fn serves_its_copy() -> Result<(), &'static str> {
    let resolve = |path: &[u8]| {
        STAGED
            .resolve(path)
            .map_err(|_| "a path the archive names did not resolve")
    };
    let (file, link, bin) = (
        resolve(b"/bin/big")?,
        resolve(b"/bin/sh")?,
        resolve(b"/bin")?,
    );
    if generation_of(file) != 2 {
        return Err("a file of the new generation names another");
    }
    if !matches!(STAGED.stat(file), Ok(s) if s.size == BIG as u64 && s.mode == 0o755 && s.sealed)
        || !matches!(STAGED.stat(link), Ok(s) if s.size == 3 && s.file_type == FileType::Symlink)
    {
        return Err("stat disagrees with the archive");
    }
    let mut back = KVec::new();
    if back.resize(BIG + 16, 0).is_err() || read_all(&STAGED, file, &mut back) != Some(BIG) {
        return Err("the file did not read back whole");
    }
    if back[..BIG]
        .iter()
        .enumerate()
        .any(|(i, &b)| b != pattern(i))
    {
        return Err("the file's bytes differ from the archive's");
    }
    let mut window = [0u8; 9];
    for offset in [65_530usize, BIG - 4] {
        let n = STAGED.read(file, offset as u64, &mut window).unwrap_or(0);
        if n != window.len().min(BIG - offset)
            || window[..n]
                .iter()
                .enumerate()
                .any(|(i, &b)| b != pattern(offset + i))
        {
            return Err("a read crossed the store's pieces wrongly");
        }
    }
    let mut target = [0u8; 16];
    if STAGED.readlink(link, &mut target) != Ok(3) || &target[..3] != b"big" {
        return Err("the symlink does not name its target");
    }
    if STAGED.lookup(bin, b"..") != Ok(STAGED.root_inode())
        || STAGED.write(file, 0, b"x") != Err(VfsError::ReadOnly)
    {
        return Err("the generation let itself be changed or walked out of");
    }
    Ok(())
}

static NAMED: BaseFs = BaseFs::new();

/// An inode of generation 1 still reads generation 1's bytes and is named by
/// its path there after generation 2, which holds the path elsewhere in its
/// index and other bytes at it, became current.
pub fn test_an_inode_names_its_generation_for_good() -> TestResult {
    let mut text = [0u8; 12];
    let Some(image) = archive(&[
        ("/aaa/first", FILE, b"before everything"),
        (INTERFACE_PATH, FILE, interface(&mut text)),
        ("/bin/tool", S_IFREG | 0o755, b"tool two"),
    ]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if NAMED.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("the first generation did not index");
    }
    let Ok(old) = NAMED.resolve(b"/bin/tool") else {
        return slopos_testing::fail!("the first generation has no /bin/tool");
    };
    if crate::vfs::orphan::open_ref(&NAMED, old).is_err() {
        return slopos_testing::fail!("could not hold the first generation open");
    }
    let outcome = (|| {
        activate(&NAMED, &image).map_err(|_| "the second generation did not activate")?;
        let new = NAMED
            .resolve(b"/bin/tool")
            .map_err(|_| "the second generation has no /bin/tool")?;
        if generation_of(old) != 1 || generation_of(new) != 2 || old == new {
            return Err("the two inodes do not name their generations");
        }
        let mut buf = [0u8; 16];
        if NAMED.read(old, 0, &mut buf) != Ok(6) || &buf[..6] != b"#!tool" {
            return Err("the old inode reads other than its generation's bytes");
        }
        if NAMED.read(new, 0, &mut buf) != Ok(8) || &buf[..8] != b"tool two" {
            return Err("the new inode reads other than its generation's bytes");
        }
        let mut path = [0u8; 16];
        if NAMED.path_of(old, &mut path) != Ok(9) || &path[..9] != b"/bin/tool" {
            return Err("the old inode is not named by its path in its generation");
        }
        Ok(())
    })();
    crate::vfs::orphan::close_ref(&NAMED, old);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(why) => slopos_testing::fail!("{}", why),
    }
}

static HELD: BaseFs = BaseFs::new();

/// A generation that is not current stays while an open file or a mapping
/// holds one of its inodes, and two reap passes after the last goes it is
/// freed: its inodes then read as gone, as an identity of it does. The
/// booted generation is never freed.
pub fn test_a_generation_stays_while_held_and_is_freed_after() -> TestResult {
    let mut text = [0u8; 12];
    let Some(image) = archive(&[
        (INTERFACE_PATH, FILE, interface(&mut text)),
        ("/bin/tool", S_IFREG | 0o755, b"tool two"),
    ]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if HELD.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("the first generation did not index");
    }
    let Ok(booted) = HELD.resolve(b"/bin/tool") else {
        return slopos_testing::fail!("the first generation has no /bin/tool");
    };
    if activate(&HELD, &image).is_err() {
        return slopos_testing::fail!("the second generation did not activate");
    }
    let Ok(tool) = HELD.resolve(b"/bin/tool") else {
        return slopos_testing::fail!("the second generation has no /bin/tool");
    };
    if crate::vfs::orphan::open_ref(&HELD, tool).is_err() {
        return slopos_testing::fail!("could not open the second generation's inode");
    }
    let map = match crate::filemap::reserve_range(
        &HELD,
        tool,
        0,
        1,
        false,
        slopos_ostd::process::AccountId::NONE,
    ) {
        Ok(map) => map,
        Err(e) => {
            crate::vfs::orphan::close_ref(&HELD, tool);
            return slopos_testing::fail!("could not map the second generation's inode: {:?}", e);
        }
    };
    let resident = || HELD.stat(tool).is_ok();
    let activated = activate(&HELD, &image).is_ok();
    let passes = |n| {
        for _ in 0..n {
            let _ = HELD.reap();
        }
    };
    passes(3);
    let held_open = resident();
    crate::vfs::orphan::close_ref(&HELD, tool);
    passes(3);
    let held_mapped = resident();
    crate::filemap::release(map, 1);
    crate::filemap::drain_pending();
    let freed_first = HELD.reap();
    let after_one = resident();
    let freed_second = HELD.reap();
    let mut path = [0u8; 16];
    if !activated {
        return slopos_testing::fail!("the third generation did not activate");
    }
    if !held_open || !held_mapped {
        return slopos_testing::fail!(
            "the generation went while held (open {}, mapped {})",
            held_open,
            held_mapped
        );
    }
    if freed_first != 0 || !after_one || freed_second != 1 || resident() {
        return slopos_testing::fail!("the generation was not freed on the second quiet pass");
    }
    if HELD.path_of(tool, &mut path) != Err(VfsError::NotFound) {
        return slopos_testing::fail!("an inode of a freed generation still has a path");
    }
    if HELD.stat(booted).is_err() {
        return slopos_testing::fail!("the booted generation was freed");
    }
    TestResult::Pass
}

/// A live process whose program is of a generation holds it, and one that
/// has exited does not.
pub fn test_a_running_program_holds_its_generation() -> TestResult {
    const NUMBER: u32 = 0x7fff_fff0;
    let Ok(process) = process_spawn(None, root_account(), slopos_ostd::process::Cred::ROOT) else {
        return slopos_testing::fail!("could not spawn a process");
    };
    let inode = core::num::NonZeroU64::new((u64::from(NUMBER) << 32) | 1).expect("nonzero");
    let before = runs_from(&BASE_FS, NUMBER);
    process.set_program(Program::Base(inode));
    let running = runs_from(&BASE_FS, NUMBER);
    process.mark_exited();
    let exited = runs_from(&BASE_FS, NUMBER);
    if let Some(handle) = process.handle() {
        process_retire(handle);
    }
    if before || !running || exited {
        return slopos_testing::fail!(
            "held before {}, while running {}, after exiting {}",
            before,
            running,
            exited
        );
    }
    TestResult::Pass
}

static MOUNTED: BaseFs = BaseFs::new();
const MOUNTS: [(&[u8], &[u8]); 6] = [
    (b"/tmp/basegen/bin", b"/bin"),
    (b"/tmp/basegen/sbin", b"/sbin"),
    (b"/tmp/basegen/lib", b"/lib"),
    (b"/tmp/basegen/ubin", b"/usr/bin"),
    (b"/tmp/basegen/share", b"/usr/share"),
    (b"/tmp/basegen/ssl", b"/etc/ssl"),
];

fn mounted_on(number: u32) -> bool {
    MOUNTS
        .iter()
        .all(|(path, _)| mount_at(path).is_some_and(|m| generation_of(m.root) == number))
}

/// Six mounts of a base move to a new generation together, each to the
/// directory it showed; one an archive lacks leaves every mount where it was.
pub fn test_the_base_mounts_switch_together() -> TestResult {
    let mut text = [0u8; 12];
    let text = interface(&mut text);
    let mut dirs: [(&str, u32, &[u8]); 7] = [
        ("/bin", DIR, b""),
        ("/sbin", DIR, b""),
        ("/lib", DIR, b""),
        ("/usr/bin", DIR, b""),
        ("/usr/share", DIR, b""),
        ("/etc/ssl", DIR, b""),
        (INTERFACE_PATH, FILE, text),
    ];
    let (Some(whole), Some(lacking)) = (archive(&dirs), {
        dirs[5] = ("/etc/other", DIR, b"");
        archive(&dirs)
    }) else {
        return slopos_testing::fail!("no room for the archives");
    };
    if vfs_init_builtin_filesystems().is_err() || MOUNTED.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("no VFS or no first generation");
    }
    let mut made = 0;
    let _ = vfs_mkdir(b"/tmp/basegen");
    for (path, _) in MOUNTS.iter() {
        let root = MOUNTED.resolve(b"/bin").unwrap_or(MOUNTED.root_inode());
        if vfs_mkdir(path).is_err() || mount_subtree(path, &MOUNTED, root, 0).is_err() {
            break;
        }
        made += 1;
    }
    let outcome = (|| {
        if made != MOUNTS.len() {
            return Err("could not mount the six fixtures");
        }
        let number = activate_on(
            &MOUNTED,
            &MOUNTS,
            &mut pieces(&whole, 509),
            whole.len() as u64,
        )
        .map_err(|_| "the whole archive did not activate")?;
        if !mounted_on(number) {
            return Err("not every mount moved to the new generation");
        }
        for (path, dir) in MOUNTS.iter() {
            if mount_at(path).map(|m| m.root) != MOUNTED.resolve(dir).ok() {
                return Err("a mount shows another directory than its own");
            }
        }
        let refused = activate_on(
            &MOUNTED,
            &MOUNTS,
            &mut pieces(&lacking, 509),
            lacking.len() as u64,
        );
        if refused != Err(ActivateError::Missing) {
            return Err("an archive lacking a mounted directory was not refused");
        }
        if !mounted_on(number) || MOUNTED.current_generation() != number {
            return Err("a refused archive moved a mount or the current generation");
        }
        let again = activate_on(
            &MOUNTED,
            &MOUNTS,
            &mut pieces(&whole, 509),
            whole.len() as u64,
        )
        .map_err(|_| "an archive after a refused one did not activate")?;
        if again != number + 2 || !mounted_on(again) {
            return Err("the refused archive kept its number or its slot");
        }
        Ok(())
    })();
    for (path, _) in MOUNTS.iter().take(made) {
        let _ = unmount(path);
    }
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(why) => slopos_testing::fail!("{}", why),
    }
}

static BUDGET: BaseFs = BaseFs::new();

/// A generation is admitted only while the copies resident with it fit the
/// budget: one held open counts against the next, which a budget that fits
/// both admits.
pub fn test_resident_copies_stay_within_the_budget() -> TestResult {
    const FILE_LEN: usize = 4000;
    let mut text = [0u8; 12];
    let mut bytes = KVec::new();
    if bytes.resize(FILE_LEN, b'x').is_err() {
        return slopos_testing::fail!("no room for the fixture");
    }
    let Some(image) = archive(&[
        (INTERFACE_PATH, FILE, interface(&mut text)),
        ("/bin/tool", S_IFREG | 0o755, &bytes),
    ]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if BUDGET.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("the first generation did not index");
    }
    let tight = (image.len() + FILE_LEN / 2) as u64;
    let activate_within = |budget| activate_on(&BUDGET, &[], &mut pieces(&image, 4093), budget);
    let Ok(second) = activate_within(tight) else {
        return slopos_testing::fail!("one copy within the budget was refused");
    };
    let Ok(held) = BUDGET.resolve(b"/bin/tool") else {
        return slopos_testing::fail!("the second generation has no /bin/tool");
    };
    if crate::vfs::orphan::open_ref(&BUDGET, held).is_err() {
        return slopos_testing::fail!("could not hold the second generation open");
    }
    let third = activate_within(tight);
    let roomy = activate_within(4 * tight);
    crate::vfs::orphan::close_ref(&BUDGET, held);
    if third != Err(ActivateError::Busy) {
        return slopos_testing::fail!("two copies past the budget were admitted: {:?}", third);
    }
    if roomy != Ok(second + 1) || BUDGET.current_generation() != second + 1 {
        return slopos_testing::fail!("a budget that fits both was refused: {:?}", roomy);
    }
    TestResult::Pass
}

static UNSERVED: BaseFs = BaseFs::new();

/// An archive naming an interface the kernel does not serve, naming none, or
/// reading past what the caller allows is refused, and the generation that
/// was current stays so.
pub fn test_an_archive_the_kernel_cannot_serve_is_refused() -> TestResult {
    let n = slopos_abi::BASE_INTERFACE + 1;
    let mut beyond = [0u8; 12];
    let mut served = [0u8; 12];
    let (Some(newer), Some(none), Some(fine)) = (
        archive(&[(INTERFACE_PATH, FILE, decimal(n, &mut beyond))]),
        archive(&[("/bin/x", FILE, b"x")]),
        archive(&[(INTERFACE_PATH, FILE, interface(&mut served))]),
    ) else {
        return slopos_testing::fail!("no room for the archives");
    };
    if UNSERVED.install(&BASE_SAMPLE, &[]).is_err() {
        return slopos_testing::fail!("the first generation did not index");
    }
    if activate(&UNSERVED, &newer) != Err(ActivateError::Unserved(Some(n))) {
        return slopos_testing::fail!("an interface past the kernel's was not refused");
    }
    if activate(&UNSERVED, &none) != Err(ActivateError::Unserved(None)) {
        return slopos_testing::fail!("an archive naming no interface was not refused");
    }
    let short = activate_on(
        &UNSERVED,
        &[],
        &mut pieces(&fine, 64),
        fine.len() as u64 / 2,
    );
    if short != Err(ActivateError::Stage(StageError::TooLarge)) {
        return slopos_testing::fail!("an archive past the limit was not refused: {:?}", short);
    }
    let truncated = stage(&mut pieces(&fine[..fine.len() - 8], 64), fine.len() as u64);
    if !matches!(truncated, Err(StageError::Archive(_))) {
        return slopos_testing::fail!("an archive cut before its trailer was staged");
    }
    if UNSERVED.current_generation() != 1 {
        return slopos_testing::fail!("a refused archive changed the current generation");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_a_staged_generation_serves_its_copy);
slopos_testing::stest!(name = test_an_inode_names_its_generation_for_good);
slopos_testing::stest!(name = test_a_generation_stays_while_held_and_is_freed_after);
slopos_testing::stest!(name = test_a_running_program_holds_its_generation);
slopos_testing::stest!(name = test_the_base_mounts_switch_together);
slopos_testing::stest!(name = test_resident_copies_stay_within_the_budget);
slopos_testing::stest!(name = test_an_archive_the_kernel_cannot_serve_is_refused);
