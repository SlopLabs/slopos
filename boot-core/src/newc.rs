//! The `newc` cpio archive a base image is, read where it lies and written
//! record by record. Every offset is checked against the archive, so a
//! malformed one is an [`Error`], never a panic.

pub const HEADER_LEN: usize = 110;
const MAGIC: &[u8] = b"070701";
const TRAILER: &[u8] = b"TRAILER!!!";
const MODE_AT: usize = 14;
const NLINK_AT: usize = 38;
const FILESIZE_AT: usize = 54;
const NAMESIZE_AT: usize = 94;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The archive ended before a record, or before the trailer.
    Truncated,
    BadMagic,
    /// A header field that is not eight hex digits, or a name of no bytes.
    BadField,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// The name as stored, without its NUL.
    pub path: &'a [u8],
    pub mode: u32,
    pub data: &'a [u8],
}

impl Entry<'_> {
    pub fn kind(&self) -> u32 {
        self.mode & slopos_abi::fs::S_IFMT
    }
}

/// The records up to the trailer, in order; an archive with no trailer ends
/// in [`Error::Truncated`].
pub fn entries(archive: &[u8]) -> Entries<'_> {
    Entries {
        archive,
        at: Some(0),
    }
}

pub struct Entries<'a> {
    archive: &'a [u8],
    at: Option<usize>,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.at.take()?;
        match record(self.archive, at) {
            Ok(Some((entry, next))) => {
                self.at = Some(next);
                Some(Ok(entry))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

fn hex8(field: &[u8]) -> Result<usize, Error> {
    field.iter().try_fold(0usize, |value, &b| {
        let digit = (b as char).to_digit(16).ok_or(Error::BadField)?;
        Ok(value << 4 | digit as usize)
    })
}

pub const fn align4(n: usize) -> Option<usize> {
    match n.checked_add(3) {
        Some(n) => Some(n & !3),
        None => None,
    }
}

/// A record's header: what follows it is `name_size` bytes of name, padding
/// to four bytes, `size` bytes of data and padding again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub mode: u32,
    pub size: usize,
    pub name_size: usize,
}

/// The header `bytes` begins with, for a reader that takes the archive a
/// record at a time rather than whole.
pub fn read_header(bytes: &[u8]) -> Result<Header, Error> {
    let header = bytes.get(..HEADER_LEN).ok_or(Error::Truncated)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic);
    }
    let field = |offset: usize| hex8(&header[offset..offset + 8]);
    let parsed = Header {
        mode: field(MODE_AT)? as u32,
        size: field(FILESIZE_AT)?,
        name_size: field(NAMESIZE_AT)?,
    };
    if parsed.name_size == 0 {
        return Err(Error::BadField);
    }
    Ok(parsed)
}

/// A stored name as a path: up to its NUL, and `None` for the trailer.
pub fn path_of(name: &[u8]) -> Option<&[u8]> {
    let path = name.split(|&b| b == 0).next().unwrap_or_default();
    (path != TRAILER).then_some(path)
}

/// The record at `at` and where the next one starts, or `None` at the
/// trailer. Inlined at opt-level 0, its checked arithmetic puts callers over
/// the kernel's stack-frame ceiling.
#[inline(never)]
fn record(archive: &[u8], at: usize) -> Result<Option<(Entry<'_>, usize)>, Error> {
    let Header {
        mode,
        size,
        name_size,
    } = read_header(archive.get(at..).ok_or(Error::Truncated)?)?;
    let name_at = at + HEADER_LEN;
    let name = name_at
        .checked_add(name_size)
        .and_then(|end| archive.get(name_at..end))
        .ok_or(Error::Truncated)?;
    let Some(path) = path_of(name) else {
        return Ok(None);
    };
    let data_at = align4(name_at + name_size).ok_or(Error::Truncated)?;
    let data_end = data_at.checked_add(size).ok_or(Error::Truncated)?;
    let data = archive.get(data_at..data_end).ok_or(Error::Truncated)?;
    let next = align4(data_end).ok_or(Error::Truncated)?;
    Ok(Some((Entry { path, mode, data }, next)))
}

/// A path's components as the base serves it: `.` and empty ones dropped,
/// so `bin/sh`, `/bin/sh` and `./bin//sh` are one path.
pub fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> + Clone {
    path.split(|&b| b == b'/')
        .filter(|name| !name.is_empty() && *name != b".")
}

pub fn same_path(a: &[u8], b: &[u8]) -> bool {
    components(a).eq(components(b))
}

/// The last record at `path`, the one the base serves; any malformed record
/// in the archive is an error.
pub fn find_last<'a>(archive: &'a [u8], path: &str) -> Result<Option<Entry<'a>>, Error> {
    let mut found = None;
    for entry in entries(archive) {
        let entry = entry?;
        if same_path(entry.path, path.as_bytes()) {
            found = Some(entry);
        }
    }
    Ok(found)
}

const ZEROS: [u8; 4] = [0; 4];

/// One record to write, at a four-byte boundary of an archive written from
/// its start: every inode number, owner and time 0, one link.
pub struct Record<'a> {
    header: [u8; HEADER_LEN],
    path: &'a [u8],
    data: &'a [u8],
}

fn header(mode: u32, size: u32, name_size: u32) -> [u8; HEADER_LEN] {
    let mut header = [b'0'; HEADER_LEN];
    header[..MAGIC.len()].copy_from_slice(MAGIC);
    let fields = [
        (MODE_AT, mode),
        (NLINK_AT, 1),
        (FILESIZE_AT, size),
        (NAMESIZE_AT, name_size),
    ];
    for (at, value) in fields {
        for (i, digit) in header[at..at + 8].iter_mut().enumerate() {
            *digit = b"0123456789ABCDEF"[((value >> (28 - 4 * i)) & 0xF) as usize];
        }
    }
    header
}

impl<'a> Record<'a> {
    /// `None` when the name or the body does not fit a header field.
    pub fn new(path: &'a [u8], mode: u32, data: &'a [u8]) -> Option<Self> {
        let size = u32::try_from(data.len()).ok()?;
        let name_size = u32::try_from(path.len().checked_add(1)?).ok()?;
        Some(Record {
            header: header(mode, size, name_size),
            path,
            data,
        })
    }

    pub fn trailer() -> Record<'static> {
        Record {
            header: header(0, 0, TRAILER.len() as u32 + 1),
            path: TRAILER,
            data: &[],
        }
    }

    /// The header, name, padding, data and padding to write, in order.
    pub fn pieces(&self) -> [&[u8]; 5] {
        let name_pad = (4 - (HEADER_LEN + self.path.len() + 1) % 4) % 4;
        let data_pad = (4 - self.data.len() % 4) % 4;
        [
            &self.header,
            self.path,
            &ZEROS[..1 + name_pad],
            self.data,
            &ZEROS[..data_pad],
        ]
    }
}

#[cfg(test)]
pub(crate) mod tests;
