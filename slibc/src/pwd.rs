//! The user and group databases: `/etc/passwd` and `/etc/group` while each is
//! a sealed regular file, which only a holder of `Seal` can make one, and the
//! base's own rows otherwise.

use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::inode_flags_ioctl::FS_IOC_GETFLAGS;
use slopos_abi::fs::{O_NOFOLLOW, O_RDONLY, S_IFMT, S_IFREG, UserFsStat};
use slopos_abi::syscall::posix::O_NONBLOCK;
use slopos_slibc_core::passwd::{BASE_GROUP, BASE_PASSWD, Group, LINE_MAX, Passwd, lines};

use crate::errno::{EINTR, EINVAL, ERANGE, errno_set};
use crate::pal::{Pal, Sys};
use crate::types::{gid_t, group, passwd, uid_t};

const PASSWD: &[u8] = b"/etc/passwd\0";
const GROUP: &[u8] = b"/etc/group\0";

/// What the kernel lets one process belong to.
const GROUPS_MAX: usize = 32;

const PTR: usize = size_of::<*mut c_char>();

/// Bytes a `getpwuid_r` buffer needs for any entry.
pub const PASSWD_BUF_MIN: usize = LINE_MAX;
/// Bytes a `getgrgid_r` buffer needs for any entry: its strings, and a
/// pointer for each member a line can name, the list's NULL and alignment.
const GROUP_BUF_MIN: usize = LINE_MAX + (LINE_MAX / 2 + 2) * PTR;

fn open_sealed(path: &[u8]) -> Option<i32> {
    let flags = (O_RDONLY | O_NOFOLLOW) as i32 | O_NONBLOCK as i32 | crate::ffi::O_CLOEXEC;
    let fd = Sys::open(path.as_ptr(), flags, 0).ok()?;
    let mut inode: u32 = 0;
    let sealed = Sys::ioctl(fd, u64::from(FS_IOC_GETFLAGS), (&raw mut inode) as u64).is_ok()
        && inode & FS_IMMUTABLE_FL != 0;
    let mut st = UserFsStat::default();
    let regular = Sys::fstat(fd, (&raw mut st).cast()).is_ok() && st.st_mode & S_IFMT == S_IFREG;
    if sealed && regular {
        Some(fd)
    } else {
        let _ = Sys::close(fd);
        None
    }
}

/// Hand `each` the database's lines until it answers true, and say whether it
/// did. A line longer than [`LINE_MAX`] is no entry.
fn scan(path: &[u8], base: &'static str, mut each: impl FnMut(&[u8]) -> bool) -> bool {
    let Some(fd) = open_sealed(path) else {
        return lines(base.as_bytes()).any(each);
    };
    let found = stream(fd, &mut each);
    let _ = Sys::close(fd);
    found
}

fn stream(fd: i32, each: &mut impl FnMut(&[u8]) -> bool) -> bool {
    let mut buf = [0u8; 2 * LINE_MAX];
    let mut have = 0usize;
    let mut overlong = false;
    loop {
        let n = match Sys::read(fd, buf[have..].as_mut_ptr(), buf.len() - have) {
            Ok(n) => n,
            Err(e) if e == EINTR => continue,
            Err(_) => return false,
        };
        if n == 0 {
            return !overlong && have > 0 && each(&buf[..have]);
        }
        have += n;
        let mut start = 0;
        while let Some(end) = buf[start..have].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + end];
            if !overlong && !line.is_empty() && each(line) {
                return true;
            }
            overlong = false;
            start += end + 1;
        }
        buf.copy_within(start..have, 0);
        have -= start;
        if have == buf.len() {
            overlong = true;
            have = 0;
        }
    }
}

/// `text` and a NUL at `buf + *at`, which has room.
unsafe fn put(buf: *mut u8, at: &mut usize, text: &[u8]) -> *mut c_char {
    let dst = buf.add(*at);
    core::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
    *dst.add(text.len()) = 0;
    *at += text.len() + 1;
    dst.cast()
}

unsafe fn fill_passwd(
    row: &Passwd<'_>,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
) -> c_int {
    if buflen < row.strings_len() {
        return ERANGE.raw();
    }
    let base = buf.cast::<u8>();
    let mut at = 0;
    (*pwd).pw_name = put(base, &mut at, row.name);
    (*pwd).pw_passwd = put(base, &mut at, row.passwd);
    (*pwd).pw_gecos = put(base, &mut at, row.gecos);
    (*pwd).pw_dir = put(base, &mut at, row.dir);
    (*pwd).pw_shell = put(base, &mut at, row.shell);
    (*pwd).pw_uid = row.uid;
    (*pwd).pw_gid = row.gid;
    0
}

/// The first entry `matches` takes, into `pwd` and `buf`.
unsafe fn find_passwd(
    mut matches: impl FnMut(&Passwd<'_>) -> bool,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut passwd,
) -> c_int {
    if pwd.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    *result = core::ptr::null_mut();
    let mut rc = 0;
    scan(PASSWD, BASE_PASSWD, |line| match Passwd::parse(line) {
        Some(row) if matches(&row) => {
            rc = fill_passwd(&row, pwd, buf, buflen);
            if rc == 0 {
                *result = pwd;
            }
            true
        }
        _ => false,
    });
    rc
}

unsafe fn fill_group(row: &Group<'_>, grp: *mut group, buf: *mut c_char, buflen: usize) -> c_int {
    let base = buf.cast::<u8>();
    let pad = base.align_offset(PTR);
    let members = row.members().count();
    let strings =
        row.name.len() + row.passwd.len() + 2 + row.members().map(|m| m.len() + 1).sum::<usize>();
    let need = pad
        .checked_add((members + 1) * PTR)
        .and_then(|n| n.checked_add(strings));
    if pad >= PTR || need.is_none_or(|need| buflen < need) {
        return ERANGE.raw();
    }
    let list = base.add(pad).cast::<*mut c_char>();
    let mut at = pad + (members + 1) * PTR;
    for (i, member) in row.members().enumerate() {
        *list.add(i) = put(base, &mut at, member);
    }
    *list.add(members) = core::ptr::null_mut();
    (*grp).gr_name = put(base, &mut at, row.name);
    (*grp).gr_passwd = put(base, &mut at, row.passwd);
    (*grp).gr_gid = row.gid;
    (*grp).gr_mem = list;
    0
}

unsafe fn find_group(
    mut matches: impl FnMut(&Group<'_>) -> bool,
    grp: *mut group,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut group,
) -> c_int {
    if grp.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    *result = core::ptr::null_mut();
    let mut rc = 0;
    scan(GROUP, BASE_GROUP, |line| match Group::parse(line) {
        Some(row) if matches(&row) => {
            rc = fill_group(&row, grp, buf, buflen);
            if rc == 0 {
                *result = grp;
            }
            true
        }
        _ => false,
    });
    rc
}

unsafe fn c_bytes<'a>(name: *const c_char) -> &'a [u8] {
    let bytes = name.cast::<u8>();
    crate::string::slice_from_cstr(bytes, crate::string::u_strlen(bytes))
}

/// `getpwuid_r(3)`. A uid with no entry is "no such entry", which POSIX
/// spells as success with a null `*result`.
///
/// # Safety
/// `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwuid_r(
    uid: uid_t,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut passwd,
) -> c_int {
    find_passwd(|row| row.uid == uid, pwd, buf, buflen, result)
}

/// `getpwnam_r(3)`, as [`getpwuid_r`].
///
/// # Safety
/// `name` is a NUL-terminated C string; `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwnam_r(
    name: *const c_char,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut passwd,
) -> c_int {
    if name.is_null() {
        return EINVAL.raw();
    }
    let name = c_bytes(name);
    find_passwd(|row| row.name == name, pwd, buf, buflen, result)
}

static mut PW_ROW: passwd = passwd {
    pw_name: core::ptr::null_mut(),
    pw_passwd: core::ptr::null_mut(),
    pw_uid: 0,
    pw_gid: 0,
    pw_gecos: core::ptr::null_mut(),
    pw_dir: core::ptr::null_mut(),
    pw_shell: core::ptr::null_mut(),
};
static mut PW_ROW_BUF: [u8; PASSWD_BUF_MIN] = [0; PASSWD_BUF_MIN];

/// Into static storage, which POSIX lets the next call overwrite: `NULL` with
/// `errno` untouched for no entry.
unsafe fn passwd_static(
    find: impl FnOnce(*mut passwd, *mut c_char, usize, *mut *mut passwd) -> c_int,
) -> *mut passwd {
    let mut result = core::ptr::null_mut();
    let rc = find(
        &raw mut PW_ROW,
        (&raw mut PW_ROW_BUF).cast(),
        PASSWD_BUF_MIN,
        &mut result,
    );
    if rc != 0 {
        errno_set(rc);
        return core::ptr::null_mut();
    }
    result
}

/// `getpwnam(3)`.
///
/// # Safety
/// `name` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwnam(name: *const c_char) -> *mut passwd {
    passwd_static(|pwd, buf, len, result| getpwnam_r(name, pwd, buf, len, result))
}

/// `getpwuid(3)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwuid(uid: uid_t) -> *mut passwd {
    passwd_static(|pwd, buf, len, result| getpwuid_r(uid, pwd, buf, len, result))
}

static mut GR_ROW: group = group {
    gr_name: core::ptr::null_mut(),
    gr_passwd: core::ptr::null_mut(),
    gr_gid: 0,
    gr_mem: core::ptr::null_mut(),
};
static mut GR_ROW_BUF: [u8; GROUP_BUF_MIN] = [0; GROUP_BUF_MIN];

unsafe fn group_static(
    find: impl FnOnce(*mut group, *mut c_char, usize, *mut *mut group) -> c_int,
) -> *mut group {
    let mut result = core::ptr::null_mut();
    let rc = find(
        &raw mut GR_ROW,
        (&raw mut GR_ROW_BUF).cast(),
        GROUP_BUF_MIN,
        &mut result,
    );
    if rc != 0 {
        errno_set(rc);
        return core::ptr::null_mut();
    }
    result
}

/// `getgrgid_r(3)`. A gid with no entry is success with a null `*result`.
///
/// # Safety
/// `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrgid_r(
    gid: gid_t,
    grp: *mut group,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut group,
) -> c_int {
    find_group(|row| row.gid == gid, grp, buf, buflen, result)
}

/// `getgrnam_r(3)`, as [`getgrgid_r`].
///
/// # Safety
/// `name` is a NUL-terminated C string; `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrnam_r(
    name: *const c_char,
    grp: *mut group,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut group,
) -> c_int {
    if name.is_null() {
        return EINVAL.raw();
    }
    let name = c_bytes(name);
    find_group(|row| row.name == name, grp, buf, buflen, result)
}

/// `getgrgid(3)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrgid(gid: gid_t) -> *mut group {
    group_static(|grp, buf, len, result| getgrgid_r(gid, grp, buf, len, result))
}

/// `getgrnam(3)`.
///
/// # Safety
/// `name` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrnam(name: *const c_char) -> *mut group {
    group_static(|grp, buf, len, result| getgrnam_r(name, grp, buf, len, result))
}

/// `group`, then each other group that lists `user`, once each, into `into`
/// as far as it holds them: how many there are. One past `into` is told
/// apart only from those `into` holds.
fn group_list(user: &[u8], group: gid_t, into: &mut [gid_t]) -> usize {
    let mut count = 0;
    let mut add = |gid: gid_t| {
        if !into[..count.min(into.len())].contains(&gid) {
            if let Some(slot) = into.get_mut(count) {
                *slot = gid;
            }
            count += 1;
        }
    };
    add(group);
    scan(GROUP, BASE_GROUP, |line| {
        if let Some(row) = Group::parse(line)
            && row.lists(user)
        {
            add(row.gid);
        }
        false
    });
    count
}

/// `initgroups(3)`: the supplementary groups become `group` and every group
/// that lists `user`, as many as the kernel holds, which only a holder of
/// `SetId` may set.
///
/// # Safety
/// `user` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn initgroups(user: *const c_char, group: gid_t) -> c_int {
    if user.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let mut gids = [group; GROUPS_MAX];
    let count = group_list(c_bytes(user), group, &mut gids);
    crate::process::ids::setgroups(count.min(GROUPS_MAX), gids.as_ptr())
}

/// `getgrouplist(3)`: `group` and every group that lists `user`, into
/// `groups`, which holds `*ngroups` of them. `*ngroups` becomes how many
/// there are, which is the answer, or `-1` when `groups` holds fewer.
///
/// # Safety
/// `user` is a NUL-terminated C string; `groups` holds `*ngroups` gids.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrouplist(
    user: *const c_char,
    group: gid_t,
    groups: *mut gid_t,
    ngroups: *mut c_int,
) -> c_int {
    if user.is_null() || ngroups.is_null() || (*ngroups > 0 && groups.is_null()) {
        errno_set(EINVAL.raw());
        return -1;
    }
    let room = usize::try_from(*ngroups).unwrap_or(0);
    let into: &mut [gid_t] = match room {
        0 => &mut [],
        // SAFETY: the caller's `groups` holds `*ngroups` gids.
        _ => core::slice::from_raw_parts_mut(groups, room),
    };
    let count = group_list(c_bytes(user), group, into);
    *ngroups = count as c_int;
    if count > room { -1 } else { count as c_int }
}

static PW_NEXT: AtomicUsize = AtomicUsize::new(0);
static GR_NEXT: AtomicUsize = AtomicUsize::new(0);

/// `setpwent(3)`: rewind [`getpwent`].
#[unsafe(no_mangle)]
pub extern "C" fn setpwent() {
    PW_NEXT.store(0, Ordering::Relaxed);
}

/// `endpwent(3)`: as [`setpwent`]; each call reads the database afresh.
#[unsafe(no_mangle)]
pub extern "C" fn endpwent() {
    setpwent();
}

/// `getpwent(3)`: the database's entries in turn, then `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwent() -> *mut passwd {
    let wanted = PW_NEXT.fetch_add(1, Ordering::Relaxed);
    let mut seen = 0;
    passwd_static(|pwd, buf, len, result| {
        find_passwd(
            |_| {
                seen += 1;
                seen > wanted
            },
            pwd,
            buf,
            len,
            result,
        )
    })
}

/// `setgrent(3)`: rewind [`getgrent`].
#[unsafe(no_mangle)]
pub extern "C" fn setgrent() {
    GR_NEXT.store(0, Ordering::Relaxed);
}

/// `endgrent(3)`: as [`setgrent`].
#[unsafe(no_mangle)]
pub extern "C" fn endgrent() {
    setgrent();
}

/// `getgrent(3)`: the database's entries in turn, then `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrent() -> *mut group {
    let wanted = GR_NEXT.fetch_add(1, Ordering::Relaxed);
    let mut seen = 0;
    group_static(|grp, buf, len, result| {
        find_group(
            |_| {
                seen += 1;
                seen > wanted
            },
            grp,
            buf,
            len,
            result,
        )
    })
}
