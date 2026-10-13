//! Permission bits against a caller's identity.

pub use slopos_ostd::process::cred::{Perm, Subject};

use crate::vfs::traits::{FileStat, FileType, Owner, VfsError, VfsResult};

/// What a permission check reads from an inode.
#[derive(Clone, Copy, Debug)]
pub struct InodePerm {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub is_dir: bool,
}

impl InodePerm {
    pub fn of(stat: &FileStat) -> Self {
        Self {
            mode: stat.mode & 0o7777,
            uid: stat.uid,
            gid: stat.gid,
            is_dir: stat.file_type == FileType::Directory,
        }
    }

    #[inline(never)]
    pub fn check(&self, who: &Subject, want: Perm) -> VfsResult<()> {
        if who.permits(self.mode, self.uid, self.gid, self.is_dir, want) {
            Ok(())
        } else {
            Err(VfsError::PermissionDenied)
        }
    }

    /// Adding or removing a name: writing and searching the directory.
    pub fn check_entries(&self, who: &Subject) -> VfsResult<()> {
        self.check(who, Perm::Write)?;
        self.check(who, Perm::Exec)
    }

    pub fn check_owner(&self, who: &Subject) -> VfsResult<()> {
        if who.owns(self.uid) {
            Ok(())
        } else {
            Err(VfsError::NotPermitted)
        }
    }

    /// A name in this directory owned by `owner` may be removed or replaced.
    pub fn check_sticky(&self, who: &Subject, owner: u32) -> VfsResult<()> {
        if who.sticky_permits(self.mode, self.uid, owner) {
            Ok(())
        } else {
            Err(VfsError::NotPermitted)
        }
    }

    /// A symlink in this directory owned by `link_owner` may be followed.
    pub fn check_follow(&self, who: &Subject, link_owner: u32) -> VfsResult<()> {
        if who.follow_permits(self.mode, self.uid, link_owner) {
            Ok(())
        } else {
            Err(VfsError::PermissionDenied)
        }
    }

    pub fn check_chown(&self, who: &Subject, uid: Option<u32>, gid: Option<u32>) -> VfsResult<()> {
        if who.may_chown(self.uid, self.gid, uid, gid) {
            Ok(())
        } else {
            Err(VfsError::NotPermitted)
        }
    }

    /// The mode an inode `who` creates in this directory, asking for `mode`,
    /// is born with.
    pub fn mode_for_child(&self, who: &Subject, mode: u16, is_dir: bool) -> u16 {
        who.child_mode(self.mode, self.gid, mode & 0o7777, is_dir)
    }

    /// Who an inode `who` creates in this directory belongs to.
    pub fn owner_for_child(&self, who: &Subject) -> Owner {
        let (uid, gid) = who.child_owner(self.mode, self.gid);
        Owner { uid, gid }
    }
}
