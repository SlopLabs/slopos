//! A machine that dies after `fsync` returns leaves its journal to the host.
//!
//! `just test-rude-exit` boots with this test named in `tests.run` and hands
//! the root disk to `scripts/check_fs_replay.sh`, which holds the host's
//! `e2fsck` to replaying what the kernel committed.

use slopos_testing::TestResult;

use crate::vfs::init::{vfs_claim_block_device, vfs_ext2_pool_claim};
use crate::vfs::{mount, vfs_init_builtin_filesystems, vfs_mkdir, vfs_open};

/// The disk `scripts/qemu_run.sh` attaches the tests image as.
const ROOT_DISK: &[u8] = b"nvme0n1";
const MOUNT_POINT: &[u8] = b"/tmp/rude-exit";
const FILE: &[u8] = b"/tmp/rude-exit/rude-exit";
/// What `just test-rude-exit` expects to read back at `/rude-exit`.
const PAYLOAD: &[u8] = b"slopos-rude-exit-v1\n";

/// `fsync` a file on the root disk with the flusher kept off the mount, so only
/// its data and journal transaction land, then end the machine.
pub fn test_ext4_rude_exit() -> TestResult {
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("the VFS did not come up");
    }
    let Ok(device) = vfs_claim_block_device(ROOT_DISK) else {
        return slopos_testing::fail!("no root disk to write to");
    };
    let Some(fs) = vfs_ext2_pool_claim() else {
        return slopos_testing::fail!("the ext2 pool handed out no instance");
    };
    fs.exclude_flusher_for_test(true);
    if fs.attach(device, false).is_err() || fs.is_read_only() {
        return slopos_testing::fail!("the root disk did not mount writable");
    }
    let _ = vfs_mkdir(MOUNT_POINT);
    if mount(MOUNT_POINT, fs, 0).is_err() {
        return slopos_testing::fail!("the root disk could not be mounted");
    }
    let Ok(handle) = vfs_open(FILE, true) else {
        return slopos_testing::fail!("the file could not be created");
    };
    if handle.write(0, PAYLOAD) != Ok(PAYLOAD.len()) {
        return slopos_testing::fail!("the payload was not written");
    }
    if handle.fs.sync_inode(handle.inode, false).is_err() {
        return slopos_testing::fail!("fsync failed");
    }
    slopos_ostd::klog_info!("RUDE_EXIT: committed");
    slopos_ostd::io::qemu_debug_exit(0);
    slopos_testing::fail!("the machine did not end")
}

slopos_testing::stest!(
    name = test_ext4_rude_exit,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
