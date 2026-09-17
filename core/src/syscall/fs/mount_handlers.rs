//! `mount(2)` and `umount2(2)`, both behind `Capability::Mount`.
//!
//! `mount(2)` cannot conjure a `&'static dyn FileSystem`, so the mountable
//! set is closed: a pooled ramfs instance, the devfs singleton, or a pooled
//! ext2 instance over a named block device. Anything else is `ENODEV`, and
//! since no member of that set reads mount options, a non-null `data` is
//! `EINVAL`.

use slopos_abi::Errno;
use slopos_abi::fs::{MNT_DETACH, MOUNT_FSTYPE_MAX, MS_RDONLY};
use slopos_fs::ext2_vfs::Ext2Mount;
use slopos_fs::vfs::VfsError;
use slopos_fs::vfs::canon::canonicalise_at;
use slopos_fs::vfs::init::{
    vfs_devfs_instance, vfs_ext2_mount_named, vfs_ext2_mounted_instance, vfs_ext2_pool_release,
    vfs_ramfs_pool_claim, vfs_ramfs_pool_release,
};
use slopos_fs::vfs::mount::{MOUNT_RDONLY, mount, mount_at, unmount, with_mount_table};
use slopos_fs::vfs::orphan::{drain_releasable, forget_filesystem, has_open_refs};
use slopos_fs::vfs::path::{RESOLVE_FOLLOW, resolve_path_at};
use slopos_fs::vfs::traits::{FileSystem, FileType, same_filesystem};

use crate::syscall::args::{UserCStr, UserPath};
use crate::syscall::fs::dirfd::with_cwd_base;

define_syscall!(syscall_mount
    (ctx,
     source: UserPath,
     target: UserPath,
     fstype: UserCStr<MOUNT_FSTYPE_MAX>,
     flags: u32,
     data: u64)
    cap(Mount)
    -> Result<(), Errno>
{
    if data != 0 {
        return Err(Errno::EINVAL);
    }
    with_cwd_base(ctx, |cwd| {
        mount_apply_at(source.as_bytes(), target.as_bytes(), cwd, fstype.as_bytes(), flags)
    })
});

define_syscall!(syscall_umount2
    (ctx, path: UserPath, flags: u32)
    cap(Mount)
    -> Result<(), Errno>
{
    with_cwd_base(ctx, |cwd| umount_path_at(path.as_bytes(), cwd, flags))
});

fn target_is_directory(path: &[u8]) -> Result<bool, Errno> {
    let resolved = resolve_path_at(path, b"/", RESOLVE_FOLLOW).map_err(vfs_errno)?;
    let stat = resolved.fs.stat(resolved.inode).map_err(vfs_errno)?;
    Ok(stat.file_type == FileType::Directory)
}

fn vfs_errno(e: VfsError) -> Errno {
    e.to_errno()
}

#[inline(never)]
pub(crate) fn mount_apply_at(
    source: &[u8],
    target: &[u8],
    cwd: &[u8],
    fstype: &[u8],
    flags: u32,
) -> Result<(), Errno> {
    let canon = canonicalise_at(target, cwd).map_err(vfs_errno)?;
    let target = canon.as_bytes();

    // The root mount is boot's: every open descriptor and every cached
    // resolution names the filesystem underneath it.
    if target == b"/" {
        return Err(Errno::EBUSY);
    }
    // `EPERM`, not `EBUSY`: covering a path a grant is keyed on would let the
    // caller hand itself that privilege, so the refusal is permanent.
    if crate::exec::grants::covers_grant_path(target) {
        return Err(Errno::EPERM);
    }
    // Before the directory check: an occupied path is by construction a valid
    // target, and asking the filesystem underneath yields a worse error.
    if mount_at(target).is_some() {
        return Err(Errno::EBUSY);
    }
    if !target_is_directory(target)? {
        return Err(Errno::ENOTDIR);
    }

    let mount_flags = if flags & MS_RDONLY != 0 {
        MOUNT_RDONLY
    } else {
        0
    };

    match fstype {
        b"ramfs" => {
            // `ENOSPC` as for a full mount table: a fixed kernel table with
            // no room left, which is not `EMFILE`'s per-process ceiling.
            let fs = vfs_ramfs_pool_claim().ok_or(Errno::ENOSPC)?;
            let instance: &'static dyn FileSystem = fs;
            match mount(target, instance, mount_flags) {
                Ok(()) => Ok(()),
                Err(e) => {
                    vfs_ramfs_pool_release(instance, false);
                    Err(vfs_errno(e))
                }
            }
        }
        b"devfs" => mount(target, vfs_devfs_instance(), mount_flags).map_err(vfs_errno),
        b"ext2" => {
            let read_only = mount_flags & MOUNT_RDONLY != 0;
            // Empty `source` means the instance this boot already attached,
            // placed at a second path. A named one gets an instance of its
            // own over that device.
            if source.is_empty() {
                let fs: &'static Ext2Mount = vfs_ext2_mounted_instance().ok_or(Errno::ENODEV)?;
                let flags = if fs.is_read_only() {
                    mount_flags | MOUNT_RDONLY
                } else {
                    mount_flags
                };
                return mount(target, fs, flags).map_err(vfs_errno);
            }
            vfs_ext2_mount_named(source, target, read_only)
                .map(|_| ())
                .map_err(vfs_errno)
        }
        _ => Err(Errno::ENODEV),
    }
}

/// How many mount points name `fs`. A mount-table scan under its own lock
/// with no filesystem call, so it is safe on any path `umount2` reaches.
fn mounts_of(fs: &'static dyn FileSystem) -> usize {
    let mut count = 0usize;
    with_mount_table(|table| {
        table.for_each_mount(&mut |mounted| {
            if same_filesystem(mounted, fs) {
                count += 1;
            }
        });
    });
    count
}

#[inline(never)]
pub(crate) fn umount_path_at(path: &[u8], cwd: &[u8], flags: u32) -> Result<(), Errno> {
    let canon = canonicalise_at(path, cwd).map_err(vfs_errno)?;
    let target = canon.as_bytes();

    if target == b"/" {
        return Err(Errno::EBUSY);
    }
    let mounted = mount_at(target).ok_or(Errno::EINVAL)?;

    // Asked of the *mount*, not of the instance: devfs and the ext2 singleton
    // can sit at several paths, and tearing an instance's records down while
    // another mount survives drops that mount's deferred frees and reports
    // `EBUSY` for a descriptor unrelated to the name being removed.
    let last_mount = mounts_of(mounted.fs) <= 1;

    // `MNT_DETACH` is cheap here because every filesystem is a `static`: the
    // descriptor holds a `&'static dyn FileSystem` and stays readable after
    // the name is gone.
    let detach = flags & MNT_DETACH != 0;
    let busy = last_mount && has_open_refs(mounted.fs);
    if busy && !detach {
        return Err(Errno::EBUSY);
    }

    if last_mount {
        let _ = mounted.fs.sync();
        // Before `forget_filesystem`: records left behind keep
        // `releasable_count()` nonzero, which keeps ext2's flusher awake for
        // an obligation nobody will ever run.
        drain_releasable(mounted.fs);
        if !busy {
            forget_filesystem(mounted.fs);
        }
    }

    unmount(target).map_err(vfs_errno)?;
    // Re-derived, because `last_mount` was sampled before the removal: two
    // tasks unmounting two paths of one instance both see two mounts, so a
    // release gated on that stale answer never runs and the instance keeps
    // its device and its exclusive write claim for the rest of the boot.
    if mounts_of(mounted.fs) == 0 && !vfs_ramfs_pool_release(mounted.fs, busy) {
        // The ext2 release is also what drops the device and gives its
        // exclusive write claim back, so the same disk can be mounted again.
        vfs_ext2_pool_release(mounted.fs, busy);
    }
    Ok(())
}
