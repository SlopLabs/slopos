//! A generation built from an archive read a record at a time, its file
//! bytes copied into a [`Store`] and nothing else kept of the archive.

use slopos_abi::Errno;
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_boot_core::base::MAX_RECORDS;
use slopos_boot_core::newc;
use slopos_ostd::KVec;

use super::StageError;
use super::index::{Data, Entry, Index};
use super::store::Store;
use crate::MAX_PATH_LEN;
use crate::cpio::{CpioError, nul_terminated};
use crate::vfs::FileType;

/// A generation not yet admitted: its index and the bytes it serves.
pub struct Staged {
    pub(super) index: Index,
    pub(super) store: Store,
}

/// Reads the archive at an offset, answering how much it read and 0 past
/// its end.
pub type Source<'a> = dyn FnMut(u64, &mut [u8]) -> Result<usize, Errno> + 'a;

fn read_exact(source: &mut Source<'_>, at: u64, buf: &mut [u8]) -> Result<(), StageError> {
    let mut done = 0;
    while done < buf.len() {
        match source(at + done as u64, &mut buf[done..]) {
            Ok(0) => return Err(StageError::Archive(newc::Error::Truncated)),
            Ok(n) => done += n,
            Err(e) => return Err(StageError::Read(e)),
        }
    }
    Ok(())
}

const fn align4(n: u64) -> u64 {
    (n + 3) & !3
}

/// A record's header, read with its name, and where its data lies.
struct Record {
    header: newc::Header,
    data_at: u64,
    end: u64,
}

fn read_record(source: &mut Source<'_>, at: u64, name: &mut [u8]) -> Result<Record, StageError> {
    let mut header = [0u8; newc::HEADER_LEN];
    read_exact(source, at, &mut header)?;
    let header = newc::read_header(&header).map_err(StageError::Archive)?;
    let name = name
        .get_mut(..header.name_size)
        .ok_or(StageError::Cpio(CpioError::NameTooLong))?;
    let name_at = at + newc::HEADER_LEN as u64;
    read_exact(source, name_at, name)?;
    let data_at = align4(name_at + header.name_size as u64);
    let end = align4(data_at + header.size as u64);
    Ok(Record {
        header,
        data_at,
        end,
    })
}

/// Nodes one generation may index: each record's, and the directories its
/// path implies that no record names.
const MAX_NODES: usize = 2 * MAX_RECORDS;

/// Build a generation from the archive `source` reads, refusing one that
/// would take more than `limit` bytes of it or more records or nodes than a
/// base may hold.
pub fn stage(source: &mut Source<'_>, limit: u64) -> Result<Staged, StageError> {
    let mut staged = Staged {
        index: Index::new().map_err(StageError::Cpio)?,
        store: Store::default(),
    };
    let mut name = KVec::<u8>::zeroed(MAX_PATH_LEN + 1).map_err(|_| StageError::NoMemory)?;
    let mut at = 0u64;
    let mut records = 0usize;
    loop {
        let record = read_record(source, at, &mut name)?;
        at = record.end;
        if at > limit {
            return Err(StageError::TooLarge);
        }
        let Some(path) = newc::path_of(&name[..record.header.name_size]) else {
            return Ok(staged);
        };
        records += 1;
        if records > MAX_RECORDS {
            return Err(StageError::TooMany);
        }
        staged.add(source, path, &record)?;
        if staged.index.len() > MAX_NODES {
            return Err(StageError::TooMany);
        }
    }
}

impl Staged {
    fn add(
        &mut self,
        source: &mut Source<'_>,
        path: &[u8],
        record: &Record,
    ) -> Result<(), StageError> {
        let mode = record.header.mode;
        let kind = match mode & S_IFMT {
            S_IFDIR => FileType::Directory,
            S_IFREG => FileType::Regular,
            S_IFLNK => FileType::Symlink,
            _ => return Ok(()),
        };
        let mut offset = record.data_at;
        let span = self.store.append(record.header.size as u64, |piece| {
            read_exact(source, offset, piece)?;
            offset += piece.len() as u64;
            Ok(())
        })?;
        let size = match kind {
            FileType::Symlink => {
                let len = span.len().min(MAX_PATH_LEN as u64) as usize;
                let mut target = KVec::<u8>::zeroed(len).map_err(|_| StageError::NoMemory)?;
                let read = self.store.read(span, 0, &mut target);
                nul_terminated(&target[..read]).len() as u64
            }
            _ => span.len(),
        };
        let entry = Entry {
            kind,
            mode: (mode & 0o7777) as u16,
            data: if kind == FileType::Directory {
                Data::NONE
            } else {
                Data::Owned(span)
            },
            size,
        };
        self.index.insert(path, entry).map_err(StageError::Cpio)
    }
}
