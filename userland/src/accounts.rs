//! The accounts store on a root and the user database written from it. The
//! record of the people goes in as sealed generations in a directory only uid
//! 0 reads ([`crate::sealed`]); `/etc/passwd`, `/etc/group` and `/etc/shadow`
//! are rendered from it and from the extensions' users, sealed too. A writer
//! holds the store's lock across reading the record and writing the
//! database, so the last one written is the newest of both; a reader takes
//! nothing, since nothing it can see is half made.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use slopos_abi::fs::{ACCOUNTS_STORE, EXTENSION_STORE, PERSON_UIDS, RAMFS_MAGIC};
use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, UserSigaction};
use slopos_accounts_core::{Accounts, Entry};

use crate::apps::extctl::store::Store as Extensions;
use crate::sealed::{
    SMALL_MAX, lock, newest, open_dir, replace_sealed, set_sealed, shown, unsealed,
};
use crate::syscall::{fs as sys_fs, process};

const RECORD: &str = "record";
const PASSWD: &str = "etc/passwd";
const GROUP: &str = "etc/group";
const SHADOW: &str = "etc/shadow";

/// The accounts store of a root. A writer holds its lock and ignores the
/// termination signals a requester relays until it is dropped, so none cuts
/// a change short.
pub struct Store {
    top: PathBuf,
    dir: PathBuf,
    _lock: Option<File>,
    held_off: Vec<(u8, UserSigaction)>,
}

impl Drop for Store {
    fn drop(&mut self) {
        for (signum, action) in &self.held_off {
            process::restore_signal(*signum, action);
        }
    }
}

fn under(top: &Path, path: &str) -> PathBuf {
    top.join(path.trim_start_matches('/'))
}

/// Whether the root at `top` keeps its records in memory, where a seal is for
/// good and nothing could be changed a second time.
pub fn in_memory(top: &Path) -> bool {
    under(top, ACCOUNTS_STORE).parent().is_some_and(on_ram)
}

fn on_ram(dir: &Path) -> bool {
    std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
        .ok()
        .and_then(|path| sys_fs::statfs_path(path.as_ptr()).ok())
        .is_some_and(|stats| stats.f_type == RAMFS_MAGIC)
}

impl Store {
    /// The store of the root at `top`, `/` for this system's, to read.
    pub fn read(top: &Path) -> Store {
        Store {
            top: top.to_path_buf(),
            dir: under(top, ACCOUNTS_STORE),
            _lock: None,
            held_off: Vec::new(),
        }
    }

    /// The store of the root at `top` to change: every other writer locked
    /// out, and its directory made uid 0's alone and sealed. A root in memory
    /// keeps a seal for good, so it keeps the one person's record.
    pub fn write(top: &Path) -> Result<Store, String> {
        let mut store = Store::read(top);
        let parent = store.dir.parent().ok_or("the store has no parent")?;
        let mut walked = PathBuf::from("/");
        for part in parent.iter().skip(1) {
            walked.push(part);
            open_dir(&walked).map_err(|e| shown(&walked, e))?;
        }
        if on_ram(parent) {
            return Err("accounts need a disk root: a RAM root keeps every seal for good".into());
        }
        let mut at = store.dir.clone().into_os_string();
        at.push(".lock");
        let at = PathBuf::from(at);
        store._lock = Some(lock(&at)?);
        store.held_off = [SIGHUP, SIGINT, SIGQUIT, SIGTERM]
            .map(|signum| (signum, process::ignore_signal_saving(signum)))
            .to_vec();
        match fs::create_dir(&store.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(shown(&store.dir, e));
            }
            _ => {}
        }
        let dir = open_dir(&store.dir).map_err(|e| shown(&store.dir, e))?;
        let meta = dir.metadata().map_err(|e| shown(&store.dir, e))?;
        if meta.uid() != 0 {
            return Err(shown(&store.dir, "not uid 0's"));
        }
        if meta.mode() & 0o777 != 0o700 {
            set_sealed(&dir, false).map_err(|e| shown(&store.dir, e))?;
            dir.set_permissions(fs::Permissions::from_mode(0o700))
                .map_err(|e| shown(&store.dir, e))?;
        }
        set_sealed(&dir, true).map_err(|e| shown(&store.dir, format!("sealing: {e}")))?;
        Ok(store)
    }

    /// The record written last, `None` on a root none was written on.
    pub fn recorded(&self) -> Result<Option<Accounts>, String> {
        if open_dir(&self.dir).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(None);
        }
        let Some((g, bytes)) = newest(&self.dir, RECORD)? else {
            return Ok(None);
        };
        let path = self.dir.join(format!("{RECORD}.{g}"));
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(Accounts::parse)
            .map(Some)
            .ok_or_else(|| shown(&path, "not an accounts record"))
    }

    /// The people as the record has them, the one person on a root none was
    /// written on.
    pub fn accounts(&self) -> Result<Accounts, String> {
        Ok(self.recorded()?.unwrap_or_else(Accounts::one_person))
    }

    /// Make `accounts` the record, which [`sync`](Self::sync) then writes
    /// the database from. Once the new generation is believed, what fails in
    /// taking older ones away or sealing the store again fails nothing.
    pub fn record(&self, accounts: &Accounts) -> Result<(), String> {
        debug_assert!(self._lock.is_some(), "a writer's store");
        let text = accounts.render();
        if text.len() as u64 > SMALL_MAX {
            return Err("the record would be too large".into());
        }
        let written = unsealed(&self.dir, || {
            crate::sealed::put_generation(&self.dir, RECORD, text.as_bytes(), 0o600)
        });
        match written {
            Err(_)
                if newest(&self.dir, RECORD)
                    .is_ok_and(|b| b.is_some_and(|(_, b)| b == text.as_bytes())) =>
            {
                Ok(())
            }
            written => written,
        }
    }

    /// Write the user database the record and the extensions' records
    /// describe: `/etc/shadow` first and from the record alone, so no
    /// extension's record keeps a changed password's old hash in place.
    pub fn sync(&self) -> Result<(), String> {
        debug_assert!(self._lock.is_some(), "a writer's store");
        let accounts = self.accounts()?;
        let write = |path: &str, text: String, mode: u32| {
            replace_sealed(&self.top.join(path), text.as_bytes(), mode)
        };
        let shadow = write(SHADOW, slopos_accounts_core::shadow(&accounts), 0o600);
        let listed = extension_entries(&self.top).and_then(|others| {
            write(
                PASSWD,
                slopos_accounts_core::passwd(&accounts, &others),
                0o644,
            )?;
            write(
                GROUP,
                slopos_accounts_core::group(&accounts, &others),
                0o644,
            )
        });
        shadow.and(listed)
    }
}

/// The entry of the user database `find` fills in, as its uid and name.
fn entry(
    find: impl FnOnce(
        *mut slopos_slibc::types::passwd,
        *mut core::ffi::c_char,
        usize,
        *mut *mut slopos_slibc::types::passwd,
    ) -> core::ffi::c_int,
) -> Option<(u32, String)> {
    let null = core::ptr::null_mut();
    let mut row = slopos_slibc::types::passwd {
        pw_name: null,
        pw_passwd: null,
        pw_uid: 0,
        pw_gid: 0,
        pw_gecos: null,
        pw_dir: null,
        pw_shell: null,
    };
    let mut buf = [0u8; slopos_slibc::pwd::PASSWD_BUF_MIN];
    let mut found = core::ptr::null_mut();
    let rc = find(&mut row, buf.as_mut_ptr().cast(), buf.len(), &mut found);
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: a row found has its name NUL-terminated in `buf`, alive here.
    let name = unsafe { core::ffi::CStr::from_ptr(row.pw_name) };
    Some((row.pw_uid, name.to_str().ok()?.to_owned()))
}

/// Whether `name` is a person's: in the record, when this process may read
/// it, or in the database, which a failed write leaves behind the record.
pub fn names_a_person(name: &str) -> bool {
    let recorded = Store::read(Path::new("/")).recorded();
    recorded.is_ok_and(|accounts| accounts.is_some_and(|a| a.person(name).is_some()))
        || uid_of(name).is_some_and(|uid| PERSON_UIDS.contains(&uid))
}

/// Whose uid the user database gives `name`.
pub fn uid_of(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a C string, and `entry` hands a row, a buffer as long
    // as it says and a result slot, all outliving the call.
    entry(|row, buf, len, found| unsafe {
        slopos_slibc::pwd::getpwnam_r(name.as_ptr(), row, buf, len, found)
    })
    .map(|(uid, _)| uid)
}

/// What the user database calls `uid`.
pub fn name_of(uid: u32) -> Option<String> {
    // SAFETY: as `uid_of`'s.
    entry(|row, buf, len, found| unsafe {
        slopos_slibc::pwd::getpwuid_r(uid, row, buf, len, found)
    })
    .map(|(_, name)| name)
}

/// The installed extensions' users, none on a root without the store.
fn extension_entries(top: &Path) -> Result<Vec<Entry>, String> {
    Ok(slopos_extension_core::accounts::entries(
        &extension_records(top)?,
    ))
}

fn extension_records(top: &Path) -> Result<Vec<slopos_extension_core::record::Record>, String> {
    if !under(top, EXTENSION_STORE).is_dir() {
        return Ok(Vec::new());
    }
    let store = Extensions::open_at(top, false)?;
    let mut out = Vec::new();
    for name in store.names()? {
        out.extend(store.record(&name)?);
    }
    Ok(out)
}

/// Every name an extension's record keeps a uid for, its removed ones
/// included, which no person takes.
pub fn extension_users(top: &Path) -> Result<Vec<String>, String> {
    Ok(extension_records(top)?
        .into_iter()
        .flat_map(|record| record.uids.into_iter().map(|(name, _)| name))
        .collect())
}
