//! `/dev/crash`: one file per record the crash store holds, named by its
//! sequence number, `-torn` after it when the record does not check out.
//! Reading a file copies the record's text; unlinking it erases the record.
//! Both take the raw-device right, since a record is the kernel's state as it
//! died. The directory exists once a store is registered.

use slopos_abi::event::MAX_TTYS;
use slopos_boot_core::crash::MAX_SLOTS;
use slopos_ostd::numfmt::fmt_u64;
use slopos_ostd::sync::OnceLock;

use super::block::NODE_INODE_BASE;
use super::{Emit, PTY_SLAVE_INODE_BASE, ROOT_INODE};
use crate::vfs::{FileStat, FileType, InodeId, VfsError, VfsResult};

pub(crate) const CRASH_DIR: InodeId = 12;
/// A record's inode is its slot's index above this.
const RECORD_INODE_BASE: InodeId = 1 << 12;
const _: () = assert!(
    PTY_SLAVE_INODE_BASE + MAX_TTYS as InodeId <= RECORD_INODE_BASE
        && RECORD_INODE_BASE + MAX_SLOTS as InodeId <= NODE_INODE_BASE
);
const TORN_SUFFIX: &[u8] = b"-torn";
const NAME_MAX: usize = 20 + TORN_SUFFIX.len();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrashRecord {
    pub sequence: u64,
    pub text_len: u64,
    pub intact: bool,
}

/// What the store behind `/dev/crash` answers. `read` and `erase` name the
/// record's sequence number as well as its slot, and the store refuses either
/// once the slot holds anything else.
pub struct CrashStoreOps {
    pub slots: fn() -> usize,
    pub record: fn(slot: usize) -> Option<CrashRecord>,
    pub read: fn(slot: usize, sequence: u64, offset: u64, buf: &mut [u8]) -> VfsResult<usize>,
    pub erase: fn(slot: usize, sequence: u64) -> VfsResult<()>,
}

static STORE: OnceLock<&'static CrashStoreOps> = OnceLock::new();

pub fn devfs_register_crash_store(ops: &'static CrashStoreOps) {
    STORE.call_once(|| ops);
}

pub(super) fn registered() -> Option<&'static CrashStoreOps> {
    STORE.get().copied()
}

fn slots(store: &CrashStoreOps) -> usize {
    (store.slots)().min(MAX_SLOTS)
}

fn slot_of(store: &CrashStoreOps, inode: InodeId) -> Option<usize> {
    let slot = usize::try_from(inode.checked_sub(RECORD_INODE_BASE)?).ok()?;
    (slot < slots(store)).then_some(slot)
}

pub(crate) fn is_record(store: &CrashStoreOps, inode: InodeId) -> bool {
    slot_of(store, inode).is_some()
}

fn record_at(store: &CrashStoreOps, inode: InodeId) -> Option<(usize, CrashRecord)> {
    let slot = slot_of(store, inode)?;
    Some((slot, (store.record)(slot)?))
}

fn name_of(record: &CrashRecord, out: &mut [u8; NAME_MAX]) -> usize {
    let mut digits = [0u8; 21];
    let number = fmt_u64(record.sequence, &mut digits);
    let mut len = number.len() - 1;
    out[..len].copy_from_slice(&number[..len]);
    if !record.intact {
        out[len..len + TORN_SUFFIX.len()].copy_from_slice(TORN_SUFFIX);
        len += TORN_SUFFIX.len();
    }
    len
}

pub(crate) fn lookup(store: &CrashStoreOps, name: &[u8]) -> VfsResult<InodeId> {
    match name {
        b"." => return Ok(CRASH_DIR),
        b".." => return Ok(ROOT_INODE),
        _ => {}
    }
    let mut spelled = [0u8; NAME_MAX];
    (0..slots(store))
        .find(|&slot| {
            (store.record)(slot).is_some_and(|record| {
                let len = name_of(&record, &mut spelled);
                spelled[..len] == *name
            })
        })
        .map(|slot| RECORD_INODE_BASE + slot as InodeId)
        .ok_or(VfsError::NotFound)
}

pub(crate) fn stat(store: &CrashStoreOps, inode: InodeId) -> Option<FileStat> {
    if inode == CRASH_DIR {
        return Some(FileStat::new_directory(inode));
    }
    let (_, record) = record_at(store, inode)?;
    let mut stat = FileStat::new_file(inode, record.text_len);
    stat.mode = 0o600;
    Some(stat)
}

/// `entitled` is the raw-device right on every production path, a parameter
/// so a test can reach the refusal.
pub(crate) fn read(
    store: &CrashStoreOps,
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
    entitled: bool,
) -> VfsResult<usize> {
    let (slot, record) = record_at(store, inode).ok_or(VfsError::NotFound)?;
    if !entitled {
        return Err(VfsError::PermissionDenied);
    }
    (store.read)(slot, record.sequence, offset, buf)
}

pub(crate) fn unlink(store: &CrashStoreOps, name: &[u8], entitled: bool) -> VfsResult<()> {
    let inode = lookup(store, name)?;
    let (slot, record) = record_at(store, inode).ok_or(VfsError::NotFound)?;
    if !entitled {
        return Err(VfsError::PermissionDenied);
    }
    (store.erase)(slot, record.sequence)
}

/// `.` and `..`, then each record, which resumes after itself at its slot
/// plus three: records come and go, and an ordinal would shift.
pub(crate) fn walk(store: &CrashStoreOps, cookie: u64, callback: Emit<'_>) -> u64 {
    let dots = [
        (&b"."[..], CRASH_DIR, FileType::Directory),
        (&b".."[..], ROOT_INODE, FileType::Directory),
    ];
    let (mut next, more) = super::walk_fixed(dots, cookie, callback);
    if !more {
        return next;
    }
    let first = usize::try_from(next.saturating_sub(2)).unwrap_or(usize::MAX);
    for slot in first..slots(store) {
        let Some(record) = (store.record)(slot) else {
            continue;
        };
        let mut name = [0u8; NAME_MAX];
        let len = name_of(&record, &mut name);
        next = slot as u64 + 3;
        let inode = RECORD_INODE_BASE + slot as InodeId;
        if !callback(next, &name[..len], inode, FileType::Regular) {
            break;
        }
    }
    next
}
