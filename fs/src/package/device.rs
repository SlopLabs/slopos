//! The block device an activation's volume is read through: a file of
//! another filesystem, every block of it checked against the image's tree on
//! its way in, as dm-verity checks a disk. The file is held open for as long
//! as the device lives, and nothing is ever written to it.
//!
//! A sealed file whose blocks are all at their home is read straight off its
//! filesystem's device by the runs it lies in, as a loop device's direct I/O
//! reads it; any other file through its filesystem. A read takes its blocks
//! in one request and checks each, the leaf over 128 of them looked up once.
//! The tree's levels above the leaves are read and checked at open and kept,
//! and the leaves last read are kept beside them, so a read reads at most one
//! tree block for each 128 blocks it covers.
//!
//! No lock is held across a read, so readers of one image run in parallel.
//! A read through the file's filesystem takes its lock, so a mount of an
//! activation takes its own lock and then that filesystem's, never the other
//! way round: nothing below a disk's filesystem reads an activation.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, KVec, klog_warn_ratelimited, lock_class};
use slopos_verity_core::{self as verity, BLOCK, Digest, Fault, Geometry};

use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::vfs::{FileRuns, FileSystem, FileType, InodeId, VfsError};

/// Leaves kept verified, besides the levels above them.
const KEPT: usize = 64;

/// Tree blocks above the leaves kept for the device's life, 1 MiB: every
/// level above the leaves of an image of up to about 15 GiB of data.
const RESIDENT_MAX: u64 = 256;

type Block = KBox<[u8; BLOCK]>;

/// Reads every device has asked of the file it is, since boot.
static READS: AtomicU64 = AtomicU64::new(0);

/// Reads every activation's device has asked of its image file, since boot.
pub fn reads() -> u64 {
    READS.load(Ordering::Relaxed)
}

/// Why a file is no image the device serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Not a regular file whose length fits a tree.
    NotImage,
    /// Its tree's top, or its first block, fails under the root named.
    Root,
    Read(VfsError),
    /// No room to hold another file open.
    TooMany,
    NoMemory,
}

/// The file an image is, read straight off its filesystem's device when it
/// can be.
struct File {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    direct: Option<FileRuns>,
}

impl File {
    /// The bytes of the file from `at`, filling `out`.
    fn read(&self, at: u64, out: &mut [u8]) -> Result<(), BlockDeviceError> {
        READS.fetch_add(1, Ordering::Relaxed);
        let Some(direct) = &self.direct else {
            return self.read_through(at, out);
        };
        let runs = direct.runs.as_slice();
        let mut done = 0;
        while done < out.len() {
            let pos = at + done as u64;
            let run = runs
                .get(runs.partition_point(|r| r.at + r.len <= pos))
                .filter(|r| r.at <= pos)
                .ok_or(BlockDeviceError::OutOfBounds)?;
            let within = pos - run.at;
            let n = ((run.len - within) as usize).min(out.len() - done);
            direct
                .device
                .read_at(run.device_at + within, &mut out[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn read_through(&self, at: u64, out: &mut [u8]) -> Result<(), BlockDeviceError> {
        let mut done = 0;
        while done < out.len() {
            match self.fs.read(self.inode, at + done as u64, &mut out[done..]) {
                Ok(0) => return Err(BlockDeviceError::OutOfBounds),
                Ok(n) => done += n,
                Err(VfsError::Interrupted) => return Err(BlockDeviceError::Interrupted),
                Err(_) => return Err(BlockDeviceError::DeviceFault),
            }
        }
        Ok(())
    }

    fn read_block(&self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), BlockDeviceError> {
        let at = block
            .checked_mul(BLOCK as u64)
            .ok_or(BlockDeviceError::OutOfBounds)?;
        self.read(at, out)
    }
}

pub(super) struct VerityFile {
    file: File,
    geometry: Geometry,
    root: Digest,
    /// The tree's blocks from its top down, as the image lays them out,
    /// checked at open.
    resident: KVec<Block>,
    /// Other tree blocks verified, the most recent last.
    kept: SpinLock<KVec<(u64, Block)>>,
}

fn block() -> Result<Block, OpenError> {
    KBox::zeroed().map_err(|_| OpenError::NoMemory)
}

/// How many of the tree's levels above the leaves are kept, from the top:
/// those that fit [`RESIDENT_MAX`] together.
fn resident_levels(geometry: &Geometry) -> usize {
    let mut kept = 0;
    let mut blocks = 0;
    for level in (1..geometry.levels()).rev() {
        let Some((_, count)) = geometry.level(level) else {
            break;
        };
        blocks += count;
        if blocks > RESIDENT_MAX {
            break;
        }
        kept += 1;
    }
    kept
}

impl VerityFile {
    /// The image in `inode` of `fs`, which the caller holds open, held open
    /// for the device's life once its tree's top and first block check out
    /// under `root`.
    #[inline(never)]
    pub(super) fn open(
        fs: &'static dyn FileSystem,
        inode: InodeId,
        root: Digest,
    ) -> Result<KBox<Self>, OpenError> {
        let stat = fs.stat(inode).map_err(OpenError::Read)?;
        if stat.file_type != FileType::Regular {
            return Err(OpenError::NotImage);
        }
        let geometry = Geometry::of_image(stat.size).ok_or(OpenError::NotImage)?;
        let kept = KVec::with_capacity(KEPT).map_err(|_| OpenError::NoMemory)?;
        let mut first = block()?;
        crate::vfs::orphan::hold_ref(fs, inode).map_err(|_| OpenError::TooMany)?;
        let direct = match fs.file_runs(inode) {
            Ok(direct) => direct,
            Err(VfsError::Interrupted) => {
                close(fs, inode);
                return Err(OpenError::Read(VfsError::Interrupted));
            }
            Err(_) => None,
        };
        let device = KBox::try_new(Self {
            file: File { fs, inode, direct },
            geometry,
            root,
            resident: KVec::new(),
            kept: SpinLock::new(kept, lock_class!("PKG_VERITY_KEPT", LOCK_LEVEL_RESOURCE)),
        });
        let Ok(mut device) = device else {
            close(fs, inode);
            return Err(OpenError::NoMemory);
        };
        device.load_resident()?;
        match device.read_blocks(0, &mut first[..]) {
            Ok(()) => Ok(device),
            Err(BlockDeviceError::IntegrityFailure) => Err(OpenError::Root),
            Err(BlockDeviceError::Interrupted) => Err(OpenError::Read(VfsError::Interrupted)),
            Err(BlockDeviceError::OutOfMemory) => Err(OpenError::NoMemory),
            Err(_) => Err(OpenError::Read(VfsError::IoError)),
        }
    }

    /// Read and check the levels [`resident_levels`] keeps, top first: in
    /// the image's order, so each block's parent is kept before it.
    #[inline(never)]
    fn load_resident(&mut self) -> Result<(), OpenError> {
        let levels = self.geometry.levels();
        let mut resident = KVec::new();
        for level in (levels - resident_levels(&self.geometry)..levels).rev() {
            let Some((_, count)) = self.geometry.level(level) else {
                break;
            };
            for index in 0..count {
                let mut out = block()?;
                let mut source = Loading {
                    file: &self.file,
                    first: self.geometry.data_blocks(),
                    resident: &mut resident,
                };
                verity::tree_block(
                    &self.geometry,
                    &self.root,
                    &mut source,
                    level,
                    index,
                    &mut out,
                )
                .map_err(|fault| match fault {
                    Fault::Mismatch(_) => OpenError::Root,
                    Fault::Read(BlockDeviceError::Interrupted) => {
                        OpenError::Read(VfsError::Interrupted)
                    }
                    Fault::Read(BlockDeviceError::OutOfMemory) => OpenError::NoMemory,
                    _ => OpenError::Read(VfsError::IoError),
                })?;
                resident.push(out).map_err(|_| OpenError::NoMemory)?;
            }
        }
        self.resident = resident;
        Ok(())
    }

    pub(super) fn data_blocks(&self) -> u64 {
        self.geometry.data_blocks()
    }

    /// What it holds for its life: itself, the map of the file's runs, the
    /// tree blocks above the leaves, and as many leaves as it keeps once read.
    pub(super) fn held_bytes(&self) -> u64 {
        let leaves = self.geometry.level(0).map_or(0, |(_, count)| count);
        let kept = leaves.min(KEPT as u64);
        let blocks = (self.resident.len() as u64 + kept) * BLOCK as u64;
        let runs = self.file.direct.as_ref().map_or(0, |direct| {
            direct.runs.capacity() * size_of::<crate::vfs::FileRun>()
        });
        let lists = self.resident.capacity() * size_of::<Block>()
            + KEPT * size_of::<(u64, Block)>()
            + size_of::<Self>()
            + runs;
        blocks + lists as u64
    }

    /// Whether the image is read straight off its filesystem's device.
    pub(super) fn reads_direct(&self) -> bool {
        self.file.direct.is_some()
    }

    /// Data blocks from `first` into `out`, whole blocks, verified; zeros
    /// when they are not.
    fn read_blocks(&self, first: u64, out: &mut [u8]) -> Result<(), BlockDeviceError> {
        let verified = self.verify(first, out);
        if verified.is_err() {
            out.fill(0);
        }
        verified
    }

    fn verify(&self, first: u64, out: &mut [u8]) -> Result<(), BlockDeviceError> {
        let mut scratch =
            KBox::<[u8; BLOCK]>::zeroed().map_err(|_| BlockDeviceError::OutOfMemory)?;
        let at = first
            .checked_mul(BLOCK as u64)
            .ok_or(BlockDeviceError::OutOfBounds)?;
        self.file.read(at, out)?;
        let mut tree = Tree { device: self };
        verity::check(
            &self.geometry,
            &self.root,
            &mut tree,
            first,
            out,
            &mut scratch,
        )
        .map_err(|fault| match fault {
            Fault::Read(e) => e,
            Fault::Mismatch(block) => {
                // Any reader of the activation's files reaches this.
                klog_warn_ratelimited!("PKG: image block {} fails its tree", block);
                BlockDeviceError::IntegrityFailure
            }
            Fault::OutOfRange => BlockDeviceError::OutOfBounds,
        })
    }

    fn resident_block(&self, block: u64) -> Option<&[u8; BLOCK]> {
        let at = block.checked_sub(self.geometry.data_blocks())?;
        self.resident.get(usize::try_from(at).ok()?).map(|b| &**b)
    }
}

/// Give back the open reference on `inode`, freeing it when it was
/// unlinked meanwhile; holds no filesystem lock.
fn close(fs: &'static dyn FileSystem, inode: InodeId) {
    if crate::vfs::orphan::close_ref(fs, inode) {
        crate::vfs::orphan::drain_or_wake(fs);
    }
}

impl Drop for VerityFile {
    fn drop(&mut self) {
        close(self.file.fs, self.file.inode);
    }
}

/// The tree as [`VerityFile::load_resident`] reads it: what it kept so far
/// is the levels above the block it reads.
struct Loading<'a> {
    file: &'a File,
    first: u64,
    resident: &'a mut KVec<Block>,
}

impl verity::Source for Loading<'_> {
    type Error = BlockDeviceError;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), BlockDeviceError> {
        self.file.read_block(block, out)
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        let kept = block
            .checked_sub(self.first)
            .and_then(|at| self.resident.get(usize::try_from(at).ok()?));
        kept.map(|kept| out.copy_from_slice(&kept[..])).is_some()
    }

    fn keep(&mut self, _block: u64, _verified: &[u8; BLOCK]) {}
}

struct Tree<'a> {
    device: &'a VerityFile,
}

impl verity::Source for Tree<'_> {
    type Error = BlockDeviceError;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), BlockDeviceError> {
        self.device.file.read_block(block, out)
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        if let Some(kept) = self.device.resident_block(block) {
            out.copy_from_slice(kept);
            return true;
        }
        let mut kept = self.device.kept.lock();
        let Some(at) = kept.iter().position(|(b, _)| *b == block) else {
            return false;
        };
        out.copy_from_slice(&kept[at].1[..]);
        if at + 1 != kept.len() {
            let entry = kept.remove(at);
            let _ = kept.push(entry);
        }
        true
    }

    fn keep(&mut self, block: u64, verified: &[u8; BLOCK]) {
        let Ok(mut copy) = KBox::<[u8; BLOCK]>::zeroed() else {
            return;
        };
        copy.copy_from_slice(verified);
        let evicted = {
            let mut kept = self.device.kept.lock();
            if kept.iter().any(|(b, _)| *b == block) {
                return;
            }
            let evicted = (kept.len() == KEPT).then(|| kept.remove(0));
            let _ = kept.push((block, copy));
            evicted
        };
        drop(evicted);
    }
}

impl BlockDevice for VerityFile {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|&end| end <= self.capacity())
            .ok_or(BlockDeviceError::OutOfBounds)?;
        let first = offset / BLOCK as u64;
        let within = (offset % BLOCK as u64) as usize;
        if within == 0 && buffer.len().is_multiple_of(BLOCK) {
            return self.read_blocks(first, buffer);
        }
        let blocks = end.div_ceil(BLOCK as u64) - first;
        let mut staged = KVec::<u8>::zeroed(blocks as usize * BLOCK)
            .map_err(|_| BlockDeviceError::OutOfMemory)?;
        self.read_blocks(first, staged.as_mut_slice())?;
        buffer.copy_from_slice(&staged.as_slice()[within..within + buffer.len()]);
        Ok(())
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn capacity(&self) -> u64 {
        self.geometry.data_blocks() * BLOCK as u64
    }

    fn logical_block_size(&self) -> u32 {
        BLOCK as u32
    }

    fn write_protected(&self) -> bool {
        true
    }
}
