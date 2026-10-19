//! Extension activations: an image, a file of another filesystem, shown
//! read-only and pinned at `/opt/<name>` and attesting its files as its own,
//! every block read verified against its tree, replaced in one step, freed
//! once nothing holds it, held to the pool, and refused a name the grammar
//! does not admit or a file that is no image the kernel serves.

use slopos_abi::extension::{DIGEST_LEN, Label, MAX_ACTIVATIONS};
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_ext4_core::bytes::{le32, put_le32};
use slopos_ext4_core::superblock::{self as sbk, off as soff, ro_compat};
use slopos_ostd::KVec;
use slopos_ostd::process::Program;
use slopos_testing::TestResult;
use slopos_verity_core::{BLOCK, Builder, Geometry};

use crate::basefs::{BaseFs, generation_of, program_of, program_path};
use crate::blockdev::MemoryBlockDevice;
use crate::ext2::cache::{BlockCache, CACHE_ENTRIES_MIN};
use crate::ext2::{Ext2Fs, NewInode};
use crate::extension::{
    self, ActivateError, IndexError, OpenError, POOL, deactivate, is_activation, label_of,
};
use crate::vfs::mount::{mount_at, mount_subtree, unmount};
use crate::vfs::path::{RESOLVE_FOLLOW, resolve_path_at};
use crate::vfs::traits::{FileSystem, InodeId, Owner, same_filesystem};
use crate::vfs::{Subject, VfsError, vfs_init_builtin_filesystems, vfs_open, vfs_unlink};
use crate::vfs_file_ops::{
    drop_vnode_for_tests, vfs_dir_handle_still_names, vnode_handle_for_tests,
};

const EXEC: u32 = S_IFREG | 0o755;
const FILE: u32 = S_IFREG | 0o644;

/// An image and the root of its tree.
pub struct Built {
    pub bytes: KVec<u8>,
    pub root: [u8; DIGEST_LEN],
}

/// An image of `records` — paths below the mount point, modes and bytes, a
/// symlink's its target — laid out by the kernel's own ext4 driver, marked
/// read-only, its tree after it.
pub fn image(records: &[(&str, u32, &[u8])]) -> Option<Built> {
    let device = volume(records)?;
    let mut bytes = KVec::new();
    device.with_buffer_mut(|buf| bytes.extend_from_slice(buf).ok())?;
    treed(bytes)
}

#[inline(never)]
fn volume(records: &[(&str, u32, &[u8])]) -> Option<MemoryBlockDevice> {
    let data: usize = records.iter().map(|r| r.2.len().div_ceil(BLOCK)).sum();
    let blocks = u32::try_from(40 + data + 2 * records.len()).ok()?;
    let spec = slopos_ext4_core::fixture::Spec {
        block_size: BLOCK as u32,
        blocks,
        inodes: 64,
        inode_size: 256,
        extents: true,
        bit64: true,
        metadata_csum: true,
        journal_blocks: 0,
        uuid: *b"slopos-ext-image",
    };
    let device =
        MemoryBlockDevice::allocate_with_block_size(blocks as usize * BLOCK, BLOCK as u32)?;
    device
        .with_buffer_mut(|buf| slopos_ext4_core::fixture::format(buf, &spec))
        .ok()?;
    mounted(&device, &mut |fs| {
        records
            .iter()
            .try_for_each(|&(path, mode, data)| put(fs, path, mode, data))
    })?;
    device.with_buffer_mut(|buf| mark_read_only(&mut buf[1024..2048]));
    Some(device)
}

/// The volume `bytes` holds with its tree after it.
#[inline(never)]
fn treed(mut bytes: KVec<u8>) -> Option<Built> {
    let geometry = Geometry::new((bytes.len() / BLOCK) as u64)?;
    let mut tree = KVec::zeroed(geometry.tree_blocks() as usize * BLOCK).ok()?;
    let mut builder = Builder::new(geometry, &mut tree)?;
    for block in bytes.chunks_exact(BLOCK) {
        builder.push(block.try_into().ok()?);
    }
    let root = builder.finish()?;
    bytes.extend_from_slice(&tree).ok()?;
    Some(Built { bytes, root })
}

fn mark_read_only(sb: &mut [u8]) {
    let features = le32(sb, soff::FEATURE_RO_COMPAT) | ro_compat::READONLY;
    put_le32(sb, soff::FEATURE_RO_COMPAT, features);
    sbk::seal(sb);
}

/// `device` mounted for `body`, in a frame of its own: the superblock, cache
/// and `Ext2Fs` leave the body no room under the 2 KiB stack gate.
#[inline(never)]
fn mounted(
    device: &MemoryBlockDevice,
    body: &mut dyn FnMut(&mut Ext2Fs<'_>) -> Option<()>,
) -> Option<()> {
    let (sb, geom) = Ext2Fs::mount_params(device).ok()?;
    let mut cache = BlockCache::new_boxed(geom.block_size(), CACHE_ENTRIES_MIN).ok()?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, geom);
    body(&mut fs)?;
    fs.sync().ok()
}

#[inline(never)]
fn put(fs: &mut Ext2Fs<'_>, path: &str, mode: u32, data: &[u8]) -> Option<()> {
    let path = path.trim_start_matches('/');
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut parent = 2;
    for dir in dirs.split('/').filter(|d| !d.is_empty()) {
        parent = match fs.lookup_child(parent, dir.as_bytes()) {
            Ok(found) => found.raw(),
            Err(_) => fs.create_directory(parent, dir.as_bytes()).ok()?,
        };
    }
    let kind = match mode & S_IFMT {
        S_IFDIR => NewInode::Directory,
        S_IFLNK => NewInode::Symlink(data),
        _ => NewInode::File,
    };
    let perm = (mode & 0o7777) as u16;
    let ino = fs
        .create_owned(parent, name.as_bytes(), kind, Owner::ROOT, perm)
        .ok()?;
    let written = mode & S_IFMT != S_IFREG || fs.write_file(ino, 0, data).ok()? == data.len();
    written.then_some(())
}

fn image_path(name: &[u8], buf: &mut [u8; 64]) -> Option<usize> {
    let prefix = b"/tmp/ext-image-";
    let len = prefix.len() + name.len();
    let out = buf.get_mut(..len)?;
    out[..prefix.len()].copy_from_slice(prefix);
    out[prefix.len()..].copy_from_slice(name);
    Some(len)
}

/// `bytes` as a file of the RAM root, activated as `name` under `root`. The
/// file's name is taken away after: the activation holds it open.
pub fn activate_bytes(
    name: &[u8],
    bytes: &[u8],
    root: [u8; DIGEST_LEN],
) -> Result<u32, ActivateError> {
    let mut buf = [0; 64];
    let len = image_path(name, &mut buf).ok_or(ActivateError::Name)?;
    let path = &buf[..len];
    let _ = vfs_unlink(path);
    let written = vfs_open(path, true).is_ok_and(|file| file.write(0, bytes) == Ok(bytes.len()));
    if !written {
        return Err(ActivateError::NoMemory);
    }
    let activated = resolve(path)
        .map_err(|e| ActivateError::Image(OpenError::Read(e)))
        .and_then(|(fs, inode)| extension::activate(name, root, fs, inode));
    let _ = vfs_unlink(path);
    activated
}

/// An image of `records`, activated as `name`.
pub fn activated(name: &[u8], records: &[(&str, u32, &[u8])]) -> Option<u32> {
    vfs_init_builtin_filesystems().ok()?;
    let built = image(records)?;
    activate_bytes(name, &built.bytes, built.root).ok()
}

fn resolve(path: &[u8]) -> Result<(&'static dyn FileSystem, InodeId), VfsError> {
    resolve_path_at(path, b"/", RESOLVE_FOLLOW, &Subject::KERNEL).map(|r| (r.fs, r.inode))
}

fn read_all(fs: &dyn FileSystem, inode: InodeId, buf: &mut [u8]) -> Option<usize> {
    fs.read(inode, 0, buf).ok()
}

fn named(program: Program, expected: &[u8]) -> bool {
    let mut buf = [0u8; 64];
    program_path(program, &mut buf).is_ok_and(|len| buf.get(..len) == Some(expected))
}

/// Free what nothing holds: two passes, as the reaper's first and second.
fn reap_all() {
    for _ in 0..2 {
        extension::pass();
    }
}

fn vacant() -> usize {
    POOL.iter().filter(|a| a.root_inode() >> 32 == 0).count()
}

/// An activation shows its image at `/opt/<name>`, read-only and pinned;
/// each file attests as the activation's program, named by its path under
/// the name and labelled with the name and digest it was activated as.
/// Deactivated, the name shows nothing, and once reaped its programs name
/// nothing either.
pub fn test_an_activation_is_shown_at_its_name_and_attests_its_files() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(built) = image(&[
        ("bin/tool", EXEC, b"tool one"),
        ("lib/libx.so", FILE, b"library"),
        ("share/link", S_IFLNK | 0o777, b"../bin/tool"),
    ]) else {
        return slopos_testing::fail!("no room for the image");
    };
    let number = match activate_bytes(b"ext-shown", &built.bytes, built.root) {
        Ok(number) => number,
        Err(e) => return slopos_testing::fail!("the image did not activate: {:?}", e),
    };
    let outcome = (|| {
        let (fs, tool) =
            resolve(b"/opt/ext-shown/bin/tool").map_err(|_| "the tool is not shown")?;
        if !is_activation(fs) || generation_of(tool) != number {
            return Err("the tool is not the activation's");
        }
        if resolve(b"/opt/ext-shown/share/link").map(|(_, at)| at) != Ok(tool) {
            return Err("a link inside the activation does not reach its file");
        }
        let mut buf = [0u8; 16];
        if read_all(fs, tool, &mut buf) != Some(8) || &buf[..8] != b"tool one" {
            return Err("the tool reads other than the image's bytes");
        }
        let stat = fs.stat(tool).map_err(|_| "no stat")?;
        if !stat.sealed || stat.uid != 0 || stat.mode & 0o7777 != 0o755 {
            return Err("the tool is not sealed, uid 0's and its mode");
        }
        let mounted = mount_at(b"/opt/ext-shown").ok_or("nothing is mounted at the name")?;
        if !mounted.read_only() || !mounted.pinned() {
            return Err("the mount is not read-only and pinned");
        }
        if vfs_open(b"/opt/ext-shown/bin/new", true).is_ok() {
            return Err("a file was made inside the activation");
        }
        let program = program_of(fs, tool);
        if !matches!(program, Program::Extension(inode) if inode.get() == tool) {
            return Err("the tool does not attest as the activation's program");
        }
        if !named(program, b"/opt/ext-shown/bin/tool") {
            return Err("the program is not named by its path under the name");
        }
        if label_of(program) != Label::new(b"ext-shown", built.root) {
            return Err("the program's label is not its activation's");
        }
        Ok(program)
    })();
    let taken = deactivate(b"ext-shown");
    let program = match outcome {
        Ok(program) => program,
        Err(why) => return slopos_testing::fail!("{}", why),
    };
    if taken.is_err() || resolve(b"/opt/ext-shown/bin/tool").is_ok() {
        return slopos_testing::fail!("the deactivated name still shows the activation");
    }
    reap_all();
    if program_path(program, &mut []).is_ok() || label_of(program).is_some() {
        return slopos_testing::fail!("the unheld activation was not freed with its names");
    }
    TestResult::Pass
}

/// A block the image's tree does not vouch for fails the read that meets it,
/// and only that read; a tree whose top does not hash to the root named is
/// refused before anything is shown.
pub fn test_a_flipped_bit_fails_the_read_that_meets_it() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(built) = image(&[
        ("bin/good", EXEC, &[b'g'; 5000]),
        ("bin/bent", EXEC, &[b'b'; 5000]),
    ]) else {
        return slopos_testing::fail!("no room for the image");
    };
    let Some(at) = built
        .bytes
        .chunks_exact(BLOCK)
        .position(|block| block.iter().all(|&b| b == b'b'))
    else {
        return slopos_testing::fail!("no block of the bent file");
    };
    let mut bent = match KVec::with_capacity(built.bytes.len()) {
        Ok(bent) => bent,
        Err(_) => return slopos_testing::fail!("no room for a copy"),
    };
    if bent.extend_from_slice(&built.bytes).is_err() {
        return slopos_testing::fail!("no room for a copy");
    }
    bent[at * BLOCK + 99] ^= 0x04;
    let shown = activate_bytes(b"ext-bent", &bent, built.root);
    let outcome = (|| {
        shown.map_err(|_| "a flip in a file's data refused the activation")?;
        let mut buf = [0u8; 8];
        let (fs, good) = resolve(b"/opt/ext-bent/bin/good").map_err(|_| "no good file")?;
        if fs.read(good, 4096, &mut buf) != Ok(buf.len()) || buf != [b'g'; 8] {
            return Err("a block the tree vouches for did not read");
        }
        let (_, bad) = resolve(b"/opt/ext-bent/bin/bent").map_err(|_| "no bent file")?;
        if fs.stat(bad).map(|s| s.size) != Ok(5000) {
            return Err("the bent file's inode did not read");
        }
        if fs.read(bad, 0, &mut buf) != Err(VfsError::IoError) {
            return Err("the flipped block read");
        }
        Ok(())
    })();
    let _ = deactivate(b"ext-bent");
    reap_all();
    if let Err(why) = outcome {
        return slopos_testing::fail!("{}", why);
    }
    let mut wrong = built.root;
    wrong[7] ^= 1;
    let refused = activate_bytes(b"ext-wrong", &built.bytes, wrong);
    if refused != Err(ActivateError::Image(OpenError::Root)) {
        return slopos_testing::fail!("an image under another root was not refused: {:?}", refused);
    }
    let mut torn = bent;
    let last = torn.len() - 1;
    torn[last] ^= 1;
    let refused = activate_bytes(b"ext-torn", &torn, built.root);
    if refused != Err(ActivateError::Image(OpenError::Root)) {
        return slopos_testing::fail!("a flip in the tree's top was not refused: {:?}", refused);
    }
    if vacant() < POOL.len() - 1 {
        return slopos_testing::fail!("a refused activation kept its instance");
    }
    TestResult::Pass
}

/// A file that is no image the kernel serves is refused: a length no tree
/// fits, one a block short, which another tree fits but not its root, a
/// volume not marked read-only, an inode at two names, and a file of an
/// activation.
pub fn test_a_file_that_is_no_image_is_refused() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(built) = image(&[("bin/tool", EXEC, b"tool")]) else {
        return slopos_testing::fail!("no room for the image");
    };
    let torn = &built.bytes[..built.bytes.len() - 100];
    let refused = activate_bytes(b"ext-torn", torn, built.root);
    if refused != Err(ActivateError::Image(OpenError::NotImage)) {
        return slopos_testing::fail!("a length no tree fits: {:?}", refused);
    }
    let short = &built.bytes[..built.bytes.len() - BLOCK];
    let refused = activate_bytes(b"ext-short", short, built.root);
    if refused != Err(ActivateError::Image(OpenError::Root)) {
        return slopos_testing::fail!("a block short of its tree: {:?}", refused);
    }
    let Some(writable) = rebuilt(&built, |volume| {
        let sb = &mut volume[1024..2048];
        let features = le32(sb, soff::FEATURE_RO_COMPAT) & !ro_compat::READONLY;
        put_le32(sb, soff::FEATURE_RO_COMPAT, features);
        sbk::seal(sb);
    }) else {
        return slopos_testing::fail!("no room for the image");
    };
    let refused = activate_bytes(b"ext-writable", &writable.bytes, writable.root);
    if !matches!(refused, Err(ActivateError::Volume(_))) {
        return slopos_testing::fail!("a volume not marked read-only: {:?}", refused);
    }
    let Some(linked) = image_linked() else {
        return slopos_testing::fail!("no room for the image");
    };
    let refused = activate_bytes(b"ext-linked", &linked.bytes, linked.root);
    if refused
        != Err(ActivateError::Index(IndexError::Shape(
            "an inode at two names",
        )))
    {
        return slopos_testing::fail!("an inode at two names: {:?}", refused);
    }
    let Some(inner) = activated(b"ext-inner", &[("share/image", FILE, &built.bytes)]) else {
        return slopos_testing::fail!("the image holding an image did not activate");
    };
    let nested = resolve(b"/opt/ext-inner/share/image")
        .map_err(|e| ActivateError::Image(OpenError::Read(e)))
        .and_then(|(fs, inode)| extension::activate(b"ext-nested", built.root, fs, inode));
    let _ = deactivate(b"ext-inner");
    reap_all();
    if nested != Err(ActivateError::Image(OpenError::NotImage)) || inner == 0 {
        return slopos_testing::fail!("an activation's file was activated: {:?}", nested);
    }
    TestResult::Pass
}

/// `built` with its volume changed by `change` and its tree built again.
fn rebuilt(built: &Built, change: impl FnOnce(&mut [u8])) -> Option<Built> {
    let geometry = Geometry::of_image(built.bytes.len() as u64)?;
    let mut bytes = KVec::new();
    let data = geometry.data_blocks() as usize * BLOCK;
    bytes.extend_from_slice(&built.bytes[..data]).ok()?;
    change(&mut bytes[..]);
    treed(bytes)
}

/// An image whose one file has a second name.
#[inline(never)]
fn image_linked() -> Option<Built> {
    let built = image(&[("bin/tool", EXEC, b"tool"), ("share/x", FILE, b"x")])?;
    rebuilt(&built, |volume| {
        if linked(volume).is_none() {
            volume.fill(0);
        }
    })
}

#[inline(never)]
fn linked(volume: &mut [u8]) -> Option<()> {
    let device = MemoryBlockDevice::allocate_with_block_size(volume.len(), BLOCK as u32)?;
    device.with_buffer_mut(|buf| {
        buf.copy_from_slice(volume);
        let sb = &mut buf[1024..2048];
        let features = le32(sb, soff::FEATURE_RO_COMPAT) & !ro_compat::READONLY;
        put_le32(sb, soff::FEATURE_RO_COMPAT, features);
        sbk::seal(sb);
    });
    mounted(&device, &mut |fs| {
        let bin = fs.lookup_child(2, b"bin").ok()?.raw();
        let tool = fs.lookup_child(bin, b"tool").ok()?.raw();
        fs.link_entry(bin, b"again", tool).ok()
    })?;
    device.with_buffer_mut(|buf| {
        mark_read_only(&mut buf[1024..2048]);
        volume.copy_from_slice(buf);
    });
    Some(())
}

/// A second activation of a name moves its mount, keeping its id, in one
/// step, and a directory handle with it; a file of the first held open still
/// reads the first's bytes, and the first is freed only once nothing holds
/// it. A name another filesystem is mounted at is neither activated over nor
/// deactivated.
pub fn test_a_replacement_switches_the_mount_in_one_step() -> TestResult {
    const BIN: &[u8] = b"/opt/ext-swap/bin";
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(first) = activated(b"ext-swap", &[("bin/tool", EXEC, b"one")]) else {
        return slopos_testing::fail!("the first did not activate");
    };
    let held = resolve(b"/opt/ext-swap/bin/tool");
    let id = mount_at(b"/opt/ext-swap").map(|m| m.id);
    let opened = held.is_ok_and(|(fs, inode)| crate::vfs::orphan::open_ref(fs, inode).is_ok());
    let dir = resolve(BIN)
        .ok()
        .and_then(|(fs, inode)| vnode_handle_for_tests(fs, inode));
    let second = activated(
        b"ext-swap",
        &[("bin/tool", EXEC, b"two"), ("bin/more", EXEC, b"more")],
    );
    let outcome = (|| {
        let (fs, old) = held.map_err(|_| "the first's tool is not shown")?;
        let second = second.ok_or("the second did not activate")?;
        if !opened {
            return Err("could not hold the first's tool open");
        }
        if !dir.is_some_and(|dir| vfs_dir_handle_still_names(dir, BIN)) {
            return Err("a directory handle did not follow the replacement");
        }
        let shown = mount_at(b"/opt/ext-swap").ok_or("the name shows nothing")?;
        if Some(shown.id) != id || generation_of(shown.root) != second || second <= first {
            return Err("the mount did not move to the second in place");
        }
        let (_, new) = resolve(b"/opt/ext-swap/bin/tool").map_err(|_| "no tool now")?;
        let mut buf = [0u8; 8];
        if generation_of(new) != second
            || read_all(fs, old, &mut buf) != Some(3)
            || &buf[..3] != b"one"
        {
            return Err("the held file does not read the first's bytes");
        }
        reap_all();
        if fs.stat(old).is_err() {
            return Err("the first was freed while held");
        }
        crate::vfs::orphan::close_ref(fs, old);
        reap_all();
        if fs.stat(old).is_ok() {
            return Err("the first was not freed once nothing held it");
        }
        Ok(())
    })();
    let _ = deactivate(b"ext-swap");
    let outlived = dir.is_some_and(|dir| vfs_dir_handle_still_names(dir, BIN));
    if let Some(dir) = dir {
        drop_vnode_for_tests(dir);
    }
    reap_all();
    if let Err(why) = outcome {
        return slopos_testing::fail!("{}", why);
    }
    if outlived {
        return slopos_testing::fail!("a directory handle outlived the deactivation");
    }
    a_mount_that_is_no_activation_stays()
}

static FOREIGN: BaseFs = BaseFs::new();

fn a_mount_that_is_no_activation_stays() -> TestResult {
    let Some(built) = image(&[("bin/tool", EXEC, b"one")]) else {
        return slopos_testing::fail!("no room for the image");
    };
    if FOREIGN.install(&super::BASE_SAMPLE, &[]).is_err()
        || mount_subtree(b"/opt/ext-foreign", &FOREIGN, FOREIGN.root_inode(), 0).is_err()
    {
        return slopos_testing::fail!("could not mount another filesystem at the name");
    }
    let over = activate_bytes(b"ext-foreign", &built.bytes, built.root);
    let taken = deactivate(b"ext-foreign");
    let stays = mount_at(b"/opt/ext-foreign").is_some_and(|m| same_filesystem(m.fs, &FOREIGN));
    let _ = unmount(b"/opt/ext-foreign");
    reap_all();
    if over != Err(ActivateError::Mounts(VfsError::Busy)) {
        return slopos_testing::fail!("an activation went over another mount: {:?}", over);
    }
    if taken != Err(ActivateError::Mounts(VfsError::NotFound)) || !stays {
        return slopos_testing::fail!("deactivating took another filesystem's mount");
    }
    if vacant() < POOL.len() {
        return slopos_testing::fail!("a refused activation kept its instance");
    }
    TestResult::Pass
}

/// The pool holds at most its number of activations at once, a deactivated
/// one held open among them, and empties again once nothing holds them.
/// What an earlier test left resident counts too.
pub fn test_activations_stay_within_the_pool() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(built) = image(&[("bin/tool", EXEC, b"t")]) else {
        return slopos_testing::fail!("no room for the image");
    };
    reap_all();
    let available = vacant();
    let held = activate_bytes(b"ext-held", &built.bytes, built.root)
        .ok()
        .and_then(|_| resolve(b"/opt/ext-held/bin/tool").ok())
        .filter(|&(fs, inode)| crate::vfs::orphan::open_ref(fs, inode).is_ok());
    let _ = deactivate(b"ext-held");
    let mut name = *b"ext-pool-00";
    let mut shown = 0;
    let mut refused = false;
    for i in 0..=MAX_ACTIVATIONS {
        name[9] = b'0' + (i / 10) as u8;
        name[10] = b'0' + (i % 10) as u8;
        match activate_bytes(&name, &built.bytes, built.root) {
            Ok(_) => shown += 1,
            Err(ActivateError::Busy) => {
                refused = true;
                break;
            }
            Err(e) => return slopos_testing::fail!("activation {} failed: {:?}", i, e),
        }
    }
    for i in 0..shown {
        name[9] = b'0' + (i / 10) as u8;
        name[10] = b'0' + (i % 10) as u8;
        let _ = deactivate(&name);
    }
    if let Some((fs, inode)) = held {
        crate::vfs::orphan::close_ref(fs, inode);
    }
    reap_all();
    if held.is_none() || !refused || shown + 1 != available {
        return slopos_testing::fail!(
            "{} shown of {} vacant beside one held before the pool refused one: {}",
            shown,
            available,
            refused
        );
    }
    if vacant() < available {
        return slopos_testing::fail!("the pool did not empty again: {} vacant", vacant());
    }
    TestResult::Pass
}

/// A name the grammar refuses is neither activated nor deactivated, and a
/// name nothing is shown at has nothing to take away.
pub fn test_a_name_outside_the_grammar_is_refused() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Some(built) = image(&[("bin/tool", EXEC, b"t")]) else {
        return slopos_testing::fail!("no room for the image");
    };
    for name in [
        &b"bin"[..],
        b"etc",
        b"Upper",
        b"dot.ted",
        b"sl/ash",
        b"..",
        b"",
        b"abcdefghijklmnopqrstuvwxyz0123456",
    ] {
        let named = resolve(b"/")
            .map_err(|e| ActivateError::Image(OpenError::Read(e)))
            .and_then(|(fs, inode)| extension::activate(name, built.root, fs, inode));
        if named != Err(ActivateError::Name) {
            return slopos_testing::fail!("an activation named {:?} was not refused", name);
        }
        if deactivate(name) != Err(ActivateError::Name) {
            return slopos_testing::fail!("deactivating {:?} was not refused", name);
        }
    }
    if deactivate(b"ext-never") != Err(ActivateError::Mounts(VfsError::NotFound)) {
        return slopos_testing::fail!("a name nothing is shown at had something taken away");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_an_activation_is_shown_at_its_name_and_attests_its_files);
slopos_testing::stest!(name = test_a_flipped_bit_fails_the_read_that_meets_it);
slopos_testing::stest!(name = test_a_file_that_is_no_image_is_refused);
slopos_testing::stest!(name = test_a_replacement_switches_the_mount_in_one_step);
slopos_testing::stest!(name = test_activations_stay_within_the_pool);
slopos_testing::stest!(name = test_a_name_outside_the_grammar_is_refused);

/// A live process running an activation's program holds that activation, as
/// one running a base program holds its generation, and an exited one does
/// not; neither holds the base's generation of the same number.
pub fn test_a_running_extension_program_holds_its_activation() -> TestResult {
    use crate::basefs::reap::runs_from;
    use slopos_ostd::process::{Cred, process_retire, process_spawn, root_account};
    const NUMBER: u32 = 0x7fff_fff1;
    let Ok(process) = process_spawn(None, root_account(), Cred::ROOT) else {
        return slopos_testing::fail!("could not spawn a process");
    };
    let inode = core::num::NonZeroU64::new((u64::from(NUMBER) << 32) | 1).expect("nonzero");
    let before = runs_from(NUMBER, true);
    process.set_program(Program::Extension(inode));
    let running = runs_from(NUMBER, true);
    let as_base = runs_from(NUMBER, false);
    process.mark_exited();
    let exited = runs_from(NUMBER, true);
    if let Some(handle) = process.handle() {
        process_retire(handle);
    }
    if before || !running || as_base || exited {
        return slopos_testing::fail!(
            "held before {}, running {}, as the base's {}, after exiting {}",
            before,
            running,
            as_base,
            exited
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_a_running_extension_program_holds_its_activation);
