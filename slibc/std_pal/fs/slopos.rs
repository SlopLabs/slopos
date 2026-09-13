#![deny(unsafe_op_in_unsafe_fn)]

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, Error, ErrorKind, IoSlice, IoSliceMut, SeekFrom};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::Dir;
use crate::sys::time::SystemTime;
use crate::sys::{AsInner, FromInner, IntoInner};
use crate::vec::Vec;

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0x40;
const O_EXCL: i32 = 0x80;
const O_TRUNC: i32 = 0x200;
const O_APPEND: i32 = 0x400;

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

const S_IFMT: u32 = 0o170000;
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFBLK: u32 = 0o060000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFSOCK: u32 = 0o140000;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;

/// `utimensat` per-field sentinel: leave this timestamp as it is.
const UTIME_OMIT: i64 = (1 << 30) - 2;

const LOCK_SH: u32 = 1;
const LOCK_EX: u32 = 2;
const LOCK_NB: u32 = 4;
const LOCK_UN: u32 = 8;

const F_OK: u32 = 0;

const DT_FIFO: u8 = 1;
const DT_CHR: u8 = 2;
const DT_DIR: u8 = 4;
const DT_BLK: u8 = 6;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;
const DT_SOCK: u8 = 12;

const ENOENT: i32 = 2;
const EAGAIN: i32 = 11;

/// Not Linux's 19: `#[repr(C)]` tail-pads the fixed header out to 24.
const DIRENT_NAME_OFFSET: usize = 24;

const DIRENT_BUF_LEN: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Mirrors `slopos_abi::fs::UserFsStat`, the Linux x86-64 `struct stat`; `std`
/// cannot depend on the ABI crate, so the asserts below pin the layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SloposStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    _pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atim: Timespec,
    st_mtim: Timespec,
    st_ctim: Timespec,
    _reserved: [i64; 3],
}

const _: () = assert!(core::mem::size_of::<SloposStat>() == 144);
const _: () = assert!(core::mem::offset_of!(SloposStat, st_mode) == 24);
const _: () = assert!(core::mem::offset_of!(SloposStat, st_size) == 48);
const _: () = assert!(core::mem::offset_of!(SloposStat, st_atim) == 72);
const _: () = assert!(core::mem::offset_of!(SloposStat, st_mtim) == 88);
const _: () = assert!(core::mem::offset_of!(SloposStat, st_ctim) == 104);

unsafe extern "C" {
    fn open(path: *const u8, flags: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64;
    fn slopos_fstat(fd: i32, stat_buf: *mut SloposStat) -> i32;
    fn slopos_fstatat(
        dirfd: i32,
        path: *const u8,
        stat_buf: *mut SloposStat,
        flags: u32,
    ) -> i32;
    fn slopos_fsync(fd: i32) -> i32;
    fn slopos_fdatasync(fd: i32) -> i32;
    fn slopos_stat(path: *const u8, stat_buf: *mut SloposStat) -> i32;
    fn slopos_mkdir(path: *const u8, mode: u32) -> i32;
    fn slopos_unlink(path: *const u8) -> i32;
    fn slopos_rmdir(path: *const u8) -> i32;
    fn slopos_rename(old: *const u8, new: *const u8) -> i32;
    fn slopos_dup(fd: i32) -> i32;
    fn slopos_symlink(target: *const u8, link_path: *const u8) -> i32;
    fn slopos_readlink(path: *const u8, buf: *mut u8, buf_len: usize) -> isize;
    fn slopos_link(old: *const u8, new: *const u8) -> i32;
    fn slopos_truncate(path: *const u8, length: u64) -> i32;
    fn slopos_ftruncate(fd: i32, length: u64) -> i32;
    fn slopos_chmod(path: *const u8, mode: u32) -> i32;
    fn slopos_fchmod(fd: i32, mode: u32) -> i32;
    fn slopos_fchmodat(dirfd: i32, path: *const u8, mode: u32, flags: u32) -> i32;
    fn slopos_utimensat(
        dirfd: i32,
        path: *const u8,
        times: *const Timespec,
        flags: u32,
    ) -> i32;
    fn slopos_getdents64(fd: i32, buf: *mut u8, buf_len: usize) -> isize;
    fn slopos_flock(fd: i32, operation: u32) -> i32;
    fn slopos_access(path: *const u8, mode: u32) -> i32;
}

pub struct File(crate::sys::fd::FileDesc);

impl File {
    pub fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }

    fn fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}

#[derive(Clone)]
pub struct FileAttr {
    stat: SloposStat,
}

pub struct ReadDir {
    root: PathBuf,
    dir: crate::sys::fd::FileDesc,
    buf: Vec<u8>,
    filled: usize,
    pos: usize,
    exhausted: bool,
}

pub struct DirEntry {
    parent: PathBuf,
    name: OsString,
    d_type: u8,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct FilePermissions {
    mode: u32,
}

#[derive(Copy, Clone, Eq)]
pub struct FileType {
    mode: u32,
}

#[derive(Debug)]
pub struct DirBuilder {
    mode: u32,
}

fn io_err_from_neg(ret: i32) -> io::Error {
    Error::from_raw_os_error(-ret)
}

fn cvt_i32(ret: i32) -> io::Result<i32> {
    if ret < 0 {
        Err(io_err_from_neg(ret))
    } else {
        Ok(ret)
    }
}

fn cvt_i64(ret: i64) -> io::Result<i64> {
    if ret < 0 {
        Err(Error::from_raw_os_error((-ret) as i32))
    } else {
        Ok(ret)
    }
}

fn cvt_isize(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(Error::from_raw_os_error((-ret) as i32))
    } else {
        Ok(ret)
    }
}

fn stat_from_path(path: &Path) -> io::Result<SloposStat> {
    let cpath = path_to_cstr(path)?;
    let mut st = SloposStat::default();
    let rc = unsafe { slopos_stat(cpath.as_ptr(), &mut st as *mut SloposStat) };
    cvt_i32(rc)?;
    Ok(st)
}

fn lstat_from_path(path: &Path) -> io::Result<SloposStat> {
    let cpath = path_to_cstr(path)?;
    let mut st = SloposStat::default();
    let rc = unsafe {
        slopos_fstatat(
            AT_FDCWD,
            cpath.as_ptr(),
            &mut st as *mut SloposStat,
            AT_SYMLINK_NOFOLLOW,
        )
    };
    cvt_i32(rc)?;
    Ok(st)
}

fn dt_to_mode(d_type: u8) -> Option<u32> {
    match d_type {
        DT_FIFO => Some(S_IFIFO),
        DT_CHR => Some(S_IFCHR),
        DT_DIR => Some(S_IFDIR),
        DT_BLK => Some(S_IFBLK),
        DT_REG => Some(S_IFREG),
        DT_LNK => Some(S_IFLNK),
        DT_SOCK => Some(S_IFSOCK),
        _ => None,
    }
}

/// `EAGAIN` means another holder, which is an answer rather than a failure.
fn try_flock(fd: i32, operation: u32) -> Result<(), TryLockError> {
    let rc = unsafe { slopos_flock(fd, operation) };
    if rc >= 0 {
        return Ok(());
    }
    if -rc == EAGAIN {
        Err(TryLockError::WouldBlock)
    } else {
        Err(TryLockError::Error(io_err_from_neg(rc)))
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut pieces: Vec<OsString> = Vec::new();

    for comp in path.components() {
        match comp {
            crate::path::Component::RootDir => {
                pieces.clear();
            }
            crate::path::Component::CurDir => {}
            crate::path::Component::ParentDir => {
                let _ = pieces.pop();
            }
            crate::path::Component::Normal(name) => {
                pieces.push(name.to_os_string());
            }
            crate::path::Component::Prefix(_) => {}
        }
    }

    out.push(Path::new("/"));
    for piece in pieces {
        out.push(piece);
    }
    out
}

fn open_flags(opts: &OpenOptions) -> io::Result<i32> {
    let access = match (opts.read, opts.write, opts.append) {
        (true, false, false) => O_RDONLY,
        (false, true, false) => O_WRONLY,
        (true, true, false) => O_RDWR,
        (false, _, true) => O_WRONLY | O_APPEND,
        (true, _, true) => O_RDWR | O_APPEND,
        (false, false, false) => {
            return Err(io::const_error!(
                ErrorKind::InvalidInput,
                "invalid access mode"
            ));
        }
    };

    match (opts.write, opts.append) {
        (true, false) => {}
        (false, false) => {
            if opts.truncate || opts.create || opts.create_new {
                return Err(io::const_error!(
                    ErrorKind::InvalidInput,
                    "invalid creation mode"
                ));
            }
        }
        (_, true) => {
            if opts.truncate && !opts.create_new {
                return Err(io::const_error!(
                    ErrorKind::InvalidInput,
                    "invalid creation mode"
                ));
            }
        }
    }

    let creation = match (opts.create, opts.truncate, opts.create_new) {
        (false, false, false) => 0,
        (true, false, false) => O_CREAT,
        (false, true, false) => O_TRUNC,
        (true, true, false) => O_CREAT | O_TRUNC,
        (_, _, true) => O_CREAT | O_EXCL,
    };

    Ok(access | creation)
}

fn os_string_from_bytes_lossy(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

pub fn path_to_cstr(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.contains(&0) {
        return Err(io::const_error!(
            ErrorKind::InvalidInput,
            "path contains NUL byte"
        ));
    }
    let mut out = bytes.to_vec();
    out.push(0);
    Ok(out)
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.stat.st_size as u64
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions {
            mode: self.stat.st_mode,
        }
    }

    pub fn file_type(&self) -> FileType {
        FileType {
            mode: self.stat.st_mode,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(
            self.stat.st_mtim.tv_sec,
            self.stat.st_mtim.tv_nsec as i32,
        ))
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(
            self.stat.st_atim.tv_sec,
            self.stat.st_atim.tv_nsec as i32,
        ))
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(
            self.stat.st_ctim.tv_sec,
            self.stat.st_ctim.tv_nsec as i32,
        ))
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o222;
        }
    }
}

impl fmt::Debug for FilePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilePermissions")
            .field("mode", &self.mode)
            .finish()
    }
}

impl FileTimes {
    pub fn set_accessed(&mut self, t: SystemTime) {
        self.accessed = Some(t);
    }

    pub fn set_modified(&mut self, t: SystemTime) {
        self.modified = Some(t);
    }

    /// `[atime, mtime]` for `utimensat`; an unset field is `UTIME_OMIT`.
    fn to_timespecs(self) -> [Timespec; 2] {
        fn conv(t: Option<SystemTime>) -> Timespec {
            match t {
                Some(t) => {
                    let (tv_sec, tv_nsec) = t.as_timespec();
                    Timespec { tv_sec, tv_nsec }
                }
                None => Timespec {
                    tv_sec: 0,
                    tv_nsec: UTIME_OMIT,
                },
            }
        }
        [conv(self.accessed), conv(self.modified)]
    }
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }
}

impl PartialEq for FileType {
    fn eq(&self, other: &Self) -> bool {
        (self.mode & S_IFMT) == (other.mode & S_IFMT)
    }
}

impl Hash for FileType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.mode & S_IFMT).hash(state);
    }
}

impl fmt::Debug for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileType")
            .field("mode", &self.mode)
            .finish()
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.root, f)
    }
}

impl ReadDir {
    fn fill(&mut self) -> io::Result<usize> {
        let n = cvt_isize(unsafe {
            slopos_getdents64(self.dir.as_raw_fd(), self.buf.as_mut_ptr(), self.buf.len())
        })? as usize;
        self.filled = n;
        self.pos = 0;
        Ok(n)
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.filled {
                if self.exhausted {
                    return None;
                }
                match self.fill() {
                    Ok(0) => {
                        self.exhausted = true;
                        return None;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.exhausted = true;
                        return Some(Err(e));
                    }
                }
            }

            let rec = &self.buf[self.pos..self.filled];
            if rec.len() <= DIRENT_NAME_OFFSET {
                self.exhausted = true;
                return Some(Err(io::const_error!(
                    ErrorKind::InvalidData,
                    "truncated directory record"
                )));
            }
            let reclen = u16::from_ne_bytes([rec[16], rec[17]]) as usize;
            if reclen <= DIRENT_NAME_OFFSET || reclen > rec.len() {
                self.exhausted = true;
                return Some(Err(io::const_error!(
                    ErrorKind::InvalidData,
                    "malformed directory record length"
                )));
            }
            let d_type = rec[18];
            let tail = &rec[DIRENT_NAME_OFFSET..reclen];
            let name_len = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
            let name = &tail[..name_len];
            self.pos += reclen;

            if name.is_empty() || name == b"." || name == b".." {
                continue;
            }
            return Some(Ok(DirEntry {
                parent: self.root.clone(),
                name: os_string_from_bytes_lossy(name),
                d_type,
            }));
        }
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        Ok(FileAttr {
            stat: lstat_from_path(&self.path())?,
        })
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        match dt_to_mode(self.d_type) {
            Some(mode) => Ok(FileType { mode }),
            None => Ok(self.metadata()?.file_type()),
        }
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }

    pub fn write(&mut self, write: bool) {
        self.write = write;
    }

    pub fn append(&mut self, append: bool) {
        self.append = append;
    }

    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }

    pub fn create(&mut self, create: bool) {
        self.create = create;
    }

    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let cpath = path_to_cstr(path)?;
        let flags = open_flags(opts)?;
        let fd = unsafe { open(cpath.as_ptr(), flags) };
        let fd = cvt_i32(fd)?;
        Ok(File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) }))
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let mut st = SloposStat::default();
        let rc = unsafe { slopos_fstat(self.fd(), &mut st as *mut SloposStat) };
        cvt_i32(rc)?;
        Ok(FileAttr { stat: st })
    }

    pub fn fsync(&self) -> io::Result<()> {
        cvt_i32(unsafe { slopos_fsync(self.fd()) })?;
        Ok(())
    }

    pub fn datasync(&self) -> io::Result<()> {
        cvt_i32(unsafe { slopos_fdatasync(self.fd()) })?;
        Ok(())
    }

    pub fn lock(&self) -> io::Result<()> {
        cvt_i32(unsafe { slopos_flock(self.fd(), LOCK_EX) })?;
        Ok(())
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        cvt_i32(unsafe { slopos_flock(self.fd(), LOCK_SH) })?;
        Ok(())
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        try_flock(self.fd(), LOCK_EX | LOCK_NB)
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        try_flock(self.fd(), LOCK_SH | LOCK_NB)
    }

    pub fn unlock(&self) -> io::Result<()> {
        cvt_i32(unsafe { slopos_flock(self.fd(), LOCK_UN) })?;
        Ok(())
    }

    pub fn truncate(&self, size: u64) -> io::Result<()> {
        cvt_i32(unsafe { slopos_ftruncate(self.fd(), size) })?;
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe { read(self.fd(), buf.as_mut_ptr(), buf.len()) };
        Ok(cvt_isize(n)? as usize)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.0.read_vectored(bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        true
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|buf| self.read(buf), cursor)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe { write(self.fd(), buf.as_ptr(), buf.len()) };
        Ok(cvt_isize(n)? as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.0.write_vectored(bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        true
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(off) => {
                if off > i64::MAX as u64 {
                    return Err(io::const_error!(
                        ErrorKind::InvalidInput,
                        "seek offset out of range"
                    ));
                }
                (off as i64, SEEK_SET)
            }
            SeekFrom::End(off) => (off, SEEK_END),
            SeekFrom::Current(off) => (off, SEEK_CUR),
        };
        let out = unsafe { slopos_lseek(self.fd(), offset, whence) };
        Ok(cvt_i64(out)? as u64)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size()))
    }

    pub fn tell(&self) -> io::Result<u64> {
        let out = unsafe { slopos_lseek(self.fd(), 0, SEEK_CUR) };
        Ok(cvt_i64(out)? as u64)
    }

    pub fn duplicate(&self) -> io::Result<File> {
        let fd = unsafe { slopos_dup(self.fd()) };
        let fd = cvt_i32(fd)?;
        Ok(File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) }))
    }

    pub fn set_permissions(&self, perm: FilePermissions) -> io::Result<()> {
        cvt_i32(unsafe { slopos_fchmod(self.fd(), perm.mode & 0o7777) })?;
        Ok(())
    }

    /// A NULL path names the descriptor itself — the `futimens` form.
    pub fn set_times(&self, times: FileTimes) -> io::Result<()> {
        let ts = times.to_timespecs();
        cvt_i32(unsafe {
            slopos_utimensat(self.fd(), core::ptr::null(), ts.as_ptr(), 0)
        })?;
        Ok(())
    }
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder { mode: 0o777 }
    }

    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        let cpath = path_to_cstr(p)?;
        let rc = unsafe { slopos_mkdir(cpath.as_ptr(), self.mode) };
        cvt_i32(rc).map(|_| ())
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("fd", &self.fd()).finish()
    }
}

impl AsInner<crate::sys::fd::FileDesc> for File {
    fn as_inner(&self) -> &crate::sys::fd::FileDesc {
        &self.0
    }
}

impl IntoInner<crate::sys::fd::FileDesc> for File {
    fn into_inner(self) -> crate::sys::fd::FileDesc {
        self.0
    }
}

impl FromInner<crate::sys::fd::FileDesc> for File {
    fn from_inner(fd: crate::sys::fd::FileDesc) -> Self {
        File(fd)
    }
}

impl AsFd for File {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for File {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for File {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) })
    }
}

pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let cpath = path_to_cstr(p)?;
    let fd = cvt_i32(unsafe { open(cpath.as_ptr(), O_RDONLY) })?;
    Ok(ReadDir {
        root: p.to_path_buf(),
        dir: unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) },
        buf: vec![0u8; DIRENT_BUF_LEN],
        filled: 0,
        pos: 0,
        exhausted: false,
    })
}

pub fn unlink(p: &Path) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    let rc = unsafe { slopos_unlink(cpath.as_ptr()) };
    cvt_i32(rc).map(|_| ())
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let cold = path_to_cstr(old)?;
    let cnew = path_to_cstr(new)?;
    let rc = unsafe { slopos_rename(cold.as_ptr(), cnew.as_ptr()) };
    cvt_i32(rc).map(|_| ())
}

pub fn set_perm(p: &Path, perm: FilePermissions) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    cvt_i32(unsafe { slopos_chmod(cpath.as_ptr(), perm.mode & 0o7777) })?;
    Ok(())
}

pub fn set_perm_nofollow(p: &Path, perm: FilePermissions) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    cvt_i32(unsafe {
        slopos_fchmodat(
            AT_FDCWD,
            cpath.as_ptr(),
            perm.mode & 0o7777,
            AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(())
}

pub fn set_times(p: &Path, times: FileTimes) -> io::Result<()> {
    utimes_at(p, times, 0)
}

pub fn set_times_nofollow(p: &Path, times: FileTimes) -> io::Result<()> {
    utimes_at(p, times, AT_SYMLINK_NOFOLLOW)
}

fn utimes_at(p: &Path, times: FileTimes, flags: u32) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    let ts = times.to_timespecs();
    cvt_i32(unsafe { slopos_utimensat(AT_FDCWD, cpath.as_ptr(), ts.as_ptr(), flags) })?;
    Ok(())
}

pub fn rmdir(p: &Path) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    cvt_i32(unsafe { slopos_rmdir(cpath.as_ptr()) })?;
    Ok(())
}

pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    for entry_res in readdir(path)? {
        let entry = entry_res?;
        let child = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            remove_dir_all(&child)?;
        } else {
            unlink(&child)?;
        }
    }
    rmdir(path)
}

pub fn exists(path: &Path) -> io::Result<bool> {
    let cpath = path_to_cstr(path)?;
    match cvt_i32(unsafe { slopos_access(cpath.as_ptr(), F_OK) }) {
        Ok(_) => Ok(true),
        Err(e) if e.raw_os_error() == Some(ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn readlink(p: &Path) -> io::Result<PathBuf> {
    let cpath = path_to_cstr(p)?;
    // A target cannot exceed the kernel's USER_PATH_MAX, so one call suffices.
    let mut buf = vec![0u8; 4096];
    let n = cvt_isize(unsafe {
        slopos_readlink(cpath.as_ptr(), buf.as_mut_ptr(), buf.len())
    })? as usize;
    buf.truncate(n);
    Ok(PathBuf::from(os_string_from_bytes_lossy(&buf)))
}

pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    let ctarget = path_to_cstr(original)?;
    let clink = path_to_cstr(link)?;
    cvt_i32(unsafe { slopos_symlink(ctarget.as_ptr(), clink.as_ptr()) })?;
    Ok(())
}

pub fn link(src: &Path, dst: &Path) -> io::Result<()> {
    let csrc = path_to_cstr(src)?;
    let cdst = path_to_cstr(dst)?;
    cvt_i32(unsafe { slopos_link(csrc.as_ptr(), cdst.as_ptr()) })?;
    Ok(())
}

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    Ok(FileAttr {
        stat: stat_from_path(p)?,
    })
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    Ok(FileAttr {
        stat: lstat_from_path(p)?,
    })
}

pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new("/").join(p)
    };
    let normalized = normalize_absolute(&abs);
    stat(&normalized)?;
    Ok(normalized)
}

pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    let mut from_opts = OpenOptions::new();
    from_opts.read(true);
    let src = File::open(from, &from_opts)?;

    let mut to_opts = OpenOptions::new();
    to_opts.write(true);
    to_opts.create(true);
    to_opts.truncate(true);
    let dst = File::open(to, &to_opts)?;

    let mut total = 0u64;
    let mut buf = [0u8; 8192];

    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }

        let mut written = 0usize;
        while written < n {
            let m = dst.write(&buf[written..n])?;
            if m == 0 {
                return Err(io::const_error!(
                    ErrorKind::WriteZero,
                    "failed to write whole buffer"
                ));
            }
            written += m;
        }

        total = total.saturating_add(n as u64);
    }

    Ok(total)
}
