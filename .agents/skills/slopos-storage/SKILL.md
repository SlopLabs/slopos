---
name: slopos-storage
description: >
  Disks and the ext4 filesystem in SlopOS: the block layer, claims, NVMe and
  virtio-blk, mounts, seals, the jbd2 journal and writeback.
  Use when a task touches drivers/src/block, drivers/src/nvme, virtio_blk,
  fs/src/ext4, ext4_vfs, ext4_dcache, ext4-core, nvme-core, build_fs_image.sh,
  or mentions /dev/disk/by-*, claims, mount, seal, journal, fsync, flusher,
  e2fsck or fsperf.
---

## Read first

- `docs/architecture/storage`: how disks are named, claimed, queued and timed out
- `docs/architecture/filesystem`: mounts, ext4 profile, seals, caches, writeback, images
- `docs/architecture/crash-recovery`: the journal, commit ordering, the at-rest mark
- `docs/architecture/usb`: USB sticks as disks

## Rules a change must keep

- A transport implements only `QueueOps`. Slots, retry, timeout, quarantine
  and the write fence stay in `block/engine.rs`; nothing allocates on the
  request path.
- Quarantined pages and tags are the device's until it completes them.
- A claim carries its disk's generation; a disk's nodes appear only once its
  partition table is in place; a block node's inode is never reused.
- Build `/dev/disk/by-id` names only in `boot_core::disk_id`, never in a driver.
- `ext4-core/profile` is the one statement of the ext4 features, held to
  what the kernel writes.
- Keep the four `EXT4_POOL_<n>` lock classes separate: a path walk holds one
  mount's lock while taking the next. Unmount must release the claim.
- A path that changes an inode record or a directory without
  `Ext4Fs::write_inode_num` or the `dir` entry helpers must bump the dcache
  generation counters itself.
- Every mutation goes through `Ext4Fs::transaction`, which re-marks the
  volume in use first. In use: `needs_recovery` before the journal goes
  live. At rest: journal emptied before the flag is cleared.
- Metadata reaches the journal before its home, file data its home before
  the commit naming it, a commit block only behind a flush. A freed block
  is not reallocated until the free is durable.
- Replay writes nothing home until every committed copy has checked out.
- Writeback is the mount's one pass, in `WRITEBACK_CHUNK` steps with the
  mount lock released between them.
- `build_fs_image.sh` never deletes a `PRESERVE_FS_IMAGE=1` image; it stops
  and names the repair. Only `just reset root` discards one.

## Checks

- `just test-host`: `ext4-core`, `nvme-core`, `boot-core` on the host
- `just test 'slopos_fs::*'`: the kernel's filesystem tests
- `just check-fs-image`: a suite's image passes `e2fsck -fn` and is at rest
- `just test-persist`: an fsynced file survives a power-off
- `just test-rude-exit`: host `e2fsck` replays what the kernel committed
- `just test-capacity`: a large volume's mount and write cost
- `just check-capture`: `run_tests gate fsperf` over a suite capture

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
