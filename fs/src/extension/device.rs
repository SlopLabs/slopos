//! The block device an activation's volume is read through: a file of
//! another filesystem, every block of it checked against the image's tree on
//! its way in, as dm-verity checks a disk. The file is held open for as long
//! as the device lives, and nothing is ever written to it.
//!
//! Its blocks are read through the file's own filesystem, so a mount of an
//! activation takes its own lock and then that filesystem's, never the other
//! way round: nothing below a disk's filesystem reads an activation.

use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::Mutex;
use slopos_ostd::{KBox, KVec, klog_warn, lock_class};
use slopos_verity_core::{self as verity, BLOCK, Digest, Fault, Geometry};

use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::vfs::{FileSystem, FileType, InodeId, VfsError};

/// Tree blocks kept verified, so a file's reads walk the tree once.
const KEPT: usize = 16;

/// Why a file is no image the device serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Not a regular file whose length fits a tree.
    NotImage,
    /// Its first block fails its tree under the root named.
    Root,
    Read(VfsError),
    /// No room to hold another file open.
    TooMany,
    NoMemory,
}

pub(super) struct VerityFile {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    geometry: Geometry,
    root: Digest,
    state: Mutex<State>,
}

struct State {
    scratch: KBox<[u8; BLOCK]>,
    bounce: KBox<[u8; BLOCK]>,
    /// Tree blocks verified, the most recent last.
    kept: KVec<(u64, KBox<[u8; BLOCK]>)>,
}

fn block() -> Result<KBox<[u8; BLOCK]>, OpenError> {
    KBox::zeroed().map_err(|_| OpenError::NoMemory)
}

impl VerityFile {
    /// The image in `inode` of `fs`, which the caller holds open, held open
    /// for the device's life once its first block checks out under `root`.
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
        let state = State {
            scratch: block()?,
            bounce: block()?,
            kept: KVec::with_capacity(KEPT).map_err(|_| OpenError::NoMemory)?,
        };
        let mut first = block()?;
        crate::vfs::orphan::hold_ref(fs, inode).map_err(|_| OpenError::TooMany)?;
        let device = KBox::try_new(Self {
            fs,
            inode,
            geometry,
            root,
            state: Mutex::new(state, lock_class!("EXT_VERITY", LOCK_LEVEL_RESOURCE)),
        });
        let Ok(device) = device else {
            close(fs, inode);
            return Err(OpenError::NoMemory);
        };
        match device.read_block(0, &mut first) {
            Ok(()) => Ok(device),
            Err(BlockDeviceError::IntegrityFailure) => Err(OpenError::Root),
            Err(BlockDeviceError::Interrupted) => Err(OpenError::Read(VfsError::Interrupted)),
            Err(_) => Err(OpenError::Read(VfsError::IoError)),
        }
    }

    pub(super) fn data_blocks(&self) -> u64 {
        self.geometry.data_blocks()
    }

    /// Data block `index`, verified, into `out`; zeros when it is not.
    fn read_block(&self, index: u64, out: &mut [u8; BLOCK]) -> Result<(), BlockDeviceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BlockDeviceError::Interrupted)?;
        let State { scratch, kept, .. } = &mut *state;
        let verified = self.verify(index, out, scratch, kept);
        if verified.is_err() {
            out.fill(0);
        }
        verified
    }

    /// The part `want` of data block `index`, from `within`.
    fn read_part(
        &self,
        index: u64,
        within: usize,
        want: &mut [u8],
    ) -> Result<(), BlockDeviceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BlockDeviceError::Interrupted)?;
        let State {
            scratch,
            bounce,
            kept,
        } = &mut *state;
        self.verify(index, bounce, scratch, kept)?;
        want.copy_from_slice(&bounce[within..within + want.len()]);
        Ok(())
    }

    fn verify(
        &self,
        index: u64,
        out: &mut [u8; BLOCK],
        scratch: &mut [u8; BLOCK],
        kept: &mut KVec<(u64, KBox<[u8; BLOCK]>)>,
    ) -> Result<(), BlockDeviceError> {
        let mut tree = Tree {
            fs: self.fs,
            inode: self.inode,
            kept,
        };
        verity::read(&self.geometry, &self.root, &mut tree, index, out, scratch).map_err(|fault| {
            match fault {
                Fault::Read(e) => e,
                Fault::Mismatch(block) => {
                    klog_warn!("EXT: image block {} fails its tree", block);
                    BlockDeviceError::IntegrityFailure
                }
                Fault::OutOfRange => BlockDeviceError::OutOfBounds,
            }
        })
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
        close(self.fs, self.inode);
    }
}

struct Tree<'a> {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    kept: &'a mut KVec<(u64, KBox<[u8; BLOCK]>)>,
}

impl verity::Source for Tree<'_> {
    type Error = BlockDeviceError;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), BlockDeviceError> {
        let at = block
            .checked_mul(BLOCK as u64)
            .ok_or(BlockDeviceError::OutOfBounds)?;
        let mut done = 0;
        while done < BLOCK {
            match self.fs.read(self.inode, at + done as u64, &mut out[done..]) {
                Ok(0) => return Err(BlockDeviceError::OutOfBounds),
                Ok(n) => done += n,
                Err(VfsError::Interrupted) => return Err(BlockDeviceError::Interrupted),
                Err(_) => return Err(BlockDeviceError::DeviceFault),
            }
        }
        Ok(())
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        let Some(at) = self.kept.iter().position(|(b, _)| *b == block) else {
            return false;
        };
        out.copy_from_slice(&self.kept[at].1[..]);
        if at + 1 != self.kept.len() {
            let entry = self.kept.remove(at);
            let _ = self.kept.push(entry);
        }
        true
    }

    fn keep(&mut self, block: u64, verified: &[u8; BLOCK]) {
        let mut copy = if self.kept.len() == KEPT {
            self.kept.remove(0).1
        } else {
            match KBox::zeroed() {
                Ok(copy) => copy,
                Err(_) => return,
            }
        };
        copy.copy_from_slice(verified);
        let _ = self.kept.push((block, copy));
    }
}

impl BlockDevice for VerityFile {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|&end| end <= self.capacity())
            .ok_or(BlockDeviceError::OutOfBounds)?;
        let mut at = offset;
        while at < end {
            let index = at / BLOCK as u64;
            let within = (at % BLOCK as u64) as usize;
            let n = ((end - at) as usize).min(BLOCK - within);
            let out = &mut buffer[(at - offset) as usize..][..n];
            match <&mut [u8; BLOCK]>::try_from(&mut *out) {
                Ok(whole) => self.read_block(index, whole)?,
                Err(_) => self.read_part(index, within, out)?,
            }
            at += n as u64;
        }
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
