use slopos_hermetic::{BootCtx, BspInit};
use slopos_ostd::klog_info;

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::exec;
use slopos_sched::scheduler::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use slopos_boot_core::layout;
use slopos_drivers::{block, crash};
use slopos_fs::blockdev::BlockDevice;
use slopos_fs::devfs::{DEV_NAME_MAX, devfs_register_crash_store};
use slopos_fs::ext2_vfs::Ext2Mount;
use slopos_fs::verity::VerityStatus;
use slopos_fs::vfs::{
    MOUNT_RDONLY, VfsError, mount, unmount, vfs_claim_block_source, vfs_ext2_pool_claim,
    vfs_ext2_pool_release, vfs_register_block_layer,
};
use slopos_fs::{RootBacking, vfs_init_builtin_filesystems_with};
use slopos_ostd::KBox;
use slopos_ostd::sync::{InitFlag, OnceLock};

/// Selected root-filesystem backing, set from the `root=` cmdline knob by
/// `early_init::boot_step_boot_config_fn` (default [`ROOT_AUTO`]).
pub const ROOT_AUTO: u8 = 0;
pub const ROOT_INITRAMFS: u8 = 1;
pub const ROOT_DISK: u8 = 2;

static ROOT_MODE: AtomicU8 = AtomicU8::new(ROOT_AUTO);

/// `root=<device>`: kept as spelled, since the disks it may name are probed
/// only after the command line is read. Unset is disk0, the first disk
/// probed.
static ROOT_DEVICE: OnceLock<&'static str> = OnceLock::new();

/// Naming a device does not force the disk root the way `root=disk` does —
/// an absent device or partition degrades to the initramfs as no disk does —
/// so this sets no [`ROOT_MODE`].
pub fn set_root_block_device(spec: &'static str) {
    ROOT_DEVICE.call_once(|| spec);
}

/// `verity=require`: a disk that is attached must come up verified, or the
/// boot step fails. No disk at all is the initramfs-only case and passes. A
/// check that can switch itself off without saying so is not a check; this is
/// how a boot asserts it is running one.
static VERITY_REQUIRED: AtomicBool = AtomicBool::new(false);

pub fn set_verity_required(required: bool) {
    VERITY_REQUIRED.store(required, Ordering::Relaxed);
}

/// Set once the initramfs is unpacked, so [`boot_step_fs_init`] demotes the ext2
/// disk to a `/mnt` secondary instead of replacing `/`.
static ROOTFS_IS_RAMFS: AtomicBool = AtomicBool::new(false);
/// The boot module's base is indexed for a disk root, which `fs init` mounts
/// it over once the disk is `/`.
static BASE_OVER_DISK: AtomicBool = AtomicBool::new(false);

pub fn set_root_mode(mode: u8) {
    ROOT_MODE.store(mode, Ordering::Relaxed);
}

static FS_HOOKS_INIT: InitFlag = InitFlag::new();

/// Idempotent: either the initramfs or the disk path may reach the VFS first.
fn register_fs_hooks() {
    if !FS_HOOKS_INIT.init_once() {
        return;
    }
    slopos_fs::fileio_register_tty_ops(&slopos_drivers::tty_file_ops::TTY_FILE_OPS);
    slopos_fs::fileio_register_socket_ops(&slopos_net::socket_file_ops::SOCKET_FILE_OPS);
    slopos_mm::filemap_hook::filemap_register_ops(slopos_fs::filemap::filemap_ops());
}

/// The block layer goes in during the *drivers* phase, not with the other FS
/// hooks: the kernel test step runs there, and a test that mounts a named
/// block device needs it before then.
fn boot_step_block_claim_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    vfs_register_block_layer(&block::VFS_OPS);
}

crate::boot_init!(
    BOOT_STEP_BLOCK_CLAIM,
    drivers,
    b"block device claim\0",
    boot_step_block_claim_fn,
    flags = boot_init_priority(88)
);

/// Armed once the disks are probed, so a panic in anything after has
/// somewhere to go.
fn boot_step_crash_store_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    let limine = (
        crate::limine_protocol::kernel_disk_guid(),
        crate::limine_protocol::kernel_path(),
    );
    let (Some(disk), Some(kernel)) = limine else {
        klog_info!("CRASH: the kernel came from no GPT disk; a panic leaves no record");
        return;
    };
    let booted = crash::Booted {
        kernel,
        cmdline: crate::limine_protocol::kernel_cmdline_str().unwrap_or(""),
        build: slopos_core::syscall::core_handlers::BUILD_TAG.unwrap_or("-"),
    };
    let store = match crash::arm(disk, &booted) {
        Ok(store) => store,
        Err(e) => {
            klog_info!("CRASH: no crash store on the boot disk: {:?}", e);
            return;
        }
    };
    devfs_register_crash_store(&crash::DEVFS_OPS);
    let held = (0..store.slots())
        .filter(|&slot| store.record(slot).is_some())
        .count();
    klog_info!(
        "CRASH: records go to {} ({} slots, {} held)",
        store.partition(),
        store.slots(),
        held
    );
}

crate::boot_init!(
    BOOT_STEP_CRASH_STORE,
    drivers,
    b"crash store\0",
    boot_step_crash_store_fn,
    flags = boot_init_priority(88)
);

/// Bring up the RAM-resident root from a Limine-loaded initramfs (cpio) module.
///
/// Runs before [`boot_step_fs_init`]; a no-op on the disk path, where that
/// step mounts the ext2 disk at `/` instead.
fn boot_step_rootfs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    let archive = crate::limine_protocol::initramfs();
    // Before the decision, not after: `root=auto` picks the disk, so the
    // outcome of attaching it is the input to the choice.
    let disk = attach_root_once();

    let mounted = matches!(disk, DiskAttachOutcome::Mounted(_));
    let writable_disk = matches!(disk, DiskAttachOutcome::Mounted(info) if !info.read_only);
    let use_initramfs = match ROOT_MODE.load(Ordering::Relaxed) {
        ROOT_INITRAMFS => true,
        ROOT_DISK => false,
        // A read-only disk falls back to the initramfs as a no-disk boot does:
        // nothing written to such a root survives, so preferring it buys no
        // persistence and costs a writable `/`. With no initramfs to fall back
        // to, a read-only disk is still a root.
        _ => !writable_disk && archive.is_some(),
    };
    if !use_initramfs {
        if !mounted {
            klog_info!("ROOTFS: no initramfs module and no mountable disk — no root to install");
            return -1;
        }
        klog_info!(
            "ROOTFS: root=disk — / is the ext2 disk{}",
            if writable_disk {
                ", and what it holds persists"
            } else {
                " (read-only)"
            }
        );
        if let Some(archive) = archive
            && writable_disk
        {
            if !install_base(archive) {
                return -1;
            }
            BASE_OVER_DISK.store(true, Ordering::Relaxed);
        }
        return 0;
    }

    let archive = match archive {
        Some(bytes) => bytes,
        None => {
            klog_info!("ROOTFS: root=initramfs requested but no initramfs module present");
            return -1;
        }
    };

    register_fs_hooks();

    // Explicitly ramfs: a writable disk may be initialised by now, and the
    // unpack must never land on it.
    if vfs_init_builtin_filesystems_with(RootBacking::Ramfs).is_err() {
        klog_info!("ROOTFS: failed to mount builtin filesystems");
        return -1;
    }

    match slopos_fs::unpack_cpio_into_root(archive) {
        Ok(entries) => {
            klog_info!(
                "ROOTFS: unpacked {} initramfs entries into RAM root",
                entries
            );
        }
        Err(e) => {
            klog_info!("ROOTFS: initramfs unpack failed: {:?}", e);
            return -1;
        }
    }
    if !install_base(archive) || mount_base() != 0 {
        return -1;
    }

    ROOTFS_IS_RAMFS.store(true, Ordering::Relaxed);
    0
}

fn install_base(archive: &'static [u8]) -> bool {
    match slopos_fs::basefs::BASE_FS.install(archive, &[]) {
        Ok(entries) => {
            klog_info!(
                "ROOTFS: the boot module's base holds {} entries ({} bytes)",
                entries,
                archive.len()
            );
            true
        }
        Err(e) => {
            klog_info!("ROOTFS: the boot module is no base: {:?}", e);
            false
        }
    }
}

fn mount_base() -> i32 {
    for dir in slopos_abi::fs::BASE_DIRS {
        match slopos_fs::vfs::init::vfs_mount_base_dir(dir.as_bytes()) {
            Ok(()) | Err(slopos_fs::vfs::VfsError::NotFound) => {}
            Err(e) => {
                klog_info!("VFS: failed to mount the base at {}: {:?}", dir, e);
                return -1;
            }
        }
    }
    klog_info!(
        "VFS: the base from the boot module is at {:?}",
        slopos_abi::fs::BASE_DIRS
    );
    0
}

/// Mount flags for the ext2 disk: read-only when the filesystem or its device
/// refuses writes, so the refusal reaches userland as `EROFS` at the VFS.
fn ext2_mount_flags(fs: &'static Ext2Mount) -> u32 {
    if fs.is_read_only() { MOUNT_RDONLY } else { 0 }
}

/// Why the root disk did not come up verified. Under `verity=require` every
/// arm is a failed boot step, not just the one that reached the trailer
/// parse.
#[derive(Debug, Clone, Copy)]
enum DiskAttachOutcome {
    NoDisk,
    Unclaimable,
    NoMemory,
    MountFailed,
    Mounted(slopos_fs::Ext2MountInfo),
}

/// The attach verdict, computed once: two boot steps need it, and
/// [`attach_root`] claims the device's exclusive write capability, which a
/// second call could not take.
static ROOT_ATTACH: OnceLock<DiskAttachOutcome> = OnceLock::new();

/// `root=initramfs` attaches no root disk: on a machine whose disks hold
/// another system, the live system mounts nothing it was not asked to.
fn attach_root_once() -> DiskAttachOutcome {
    if ROOT_MODE.load(Ordering::Relaxed) == ROOT_INITRAMFS {
        return DiskAttachOutcome::NoDisk;
    }
    ROOT_ATTACH.call_once(attach_root);
    ROOT_ATTACH
        .get()
        .copied()
        .unwrap_or(DiskAttachOutcome::NoDisk)
}

/// The pooled ext2 instance the root disk is attached to. Named here because
/// two boot steps mount it and `mount(2)` must not be able to claim it.
static ROOT_EXT2: OnceLock<&'static Ext2Mount> = OnceLock::new();

/// The instance `/` (or `/mnt`) is backed by, once [`attach_root`] has run.
fn root_ext2() -> Option<&'static Ext2Mount> {
    ROOT_EXT2.get().copied()
}

/// The device `root=` names, disk0 when it names none, claimed for writing:
/// the window and the name of the node it claimed.
fn claim_root(
    name: &mut [u8; DEV_NAME_MAX],
) -> Result<(KBox<dyn BlockDevice + Send + Sync>, usize), DiskAttachOutcome> {
    let disk0 = block::disk_name(0);
    let (source, named) = match (ROOT_DEVICE.get(), &disk0) {
        (Some(spec), _) => (spec.as_bytes(), true),
        (None, Some(disk0)) => (disk0.as_bytes(), false),
        (None, None) => return Err(DiskAttachOutcome::NoDisk),
    };
    let shown = core::str::from_utf8(source).unwrap_or("?");
    vfs_claim_block_source(source, name).map_err(|e| match e {
        VfsError::NoSpace => DiskAttachOutcome::NoMemory,
        VfsError::NotFound | VfsError::InvalidArgument => {
            if named {
                klog_info!("FS: root={} names no usable block device ({:?})", shown, e);
            }
            DiskAttachOutcome::NoDisk
        }
        e => {
            klog_info!("FS: could not claim {}: {:?}", shown, e);
            DiskAttachOutcome::Unclaimable
        }
    })
}

fn attach_root() -> DiskAttachOutcome {
    let mut buf = [0u8; DEV_NAME_MAX];
    let (window, len) = match claim_root(&mut buf) {
        Ok(claimed) => claimed,
        Err(outcome) => return outcome,
    };
    let name = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    let Some(fs) = vfs_ext2_pool_claim() else {
        klog_info!("FS: no ext2 instance left for the root disk");
        return DiskAttachOutcome::NoMemory;
    };
    match fs.attach(window, false) {
        Ok(info) => {
            klog_info!("FS: the root disk is {}", name);
            ROOT_EXT2.call_once(|| fs);
            DiskAttachOutcome::Mounted(info)
        }
        Err(e) => {
            klog_info!("FS: {} found but ext2 init failed: {:?}", name, e);
            vfs_ext2_pool_release(fs, false);
            DiskAttachOutcome::MountFailed
        }
    }
}

fn boot_step_fs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    register_fs_hooks();

    let outcome = attach_root_once();
    if let DiskAttachOutcome::Mounted(info) = outcome {
        klog_info!(
            "FS: ext2 initialized from the root disk ({}, verity {})",
            if info.read_only {
                "read-only"
            } else {
                "read-write"
            },
            match info.verity {
                VerityStatus::Absent => "absent",
                VerityStatus::Verified { .. } => "enabled",
                VerityStatus::VerifiedWritable { .. } => "enabled (writable)",
            },
        );
        if info.orphans_drained > 0 {
            klog_info!(
                "FS: reclaimed {} inode(s) the previous boot left unlinked-but-open",
                info.orphans_drained
            );
        }
        if info.check_overdue {
            klog_info!(
                "FS: the image is due a full check by its own mount-count or check-interval \
                 rule — run `e2fsck -f` on the host"
            );
        }
    }
    // Absence is not an error: on real hardware the root came from the
    // initramfs. A disk that is there, though, must come up verified when the
    // boot said so — and a disk that is there but could not be mounted is
    // exactly the case the knob exists to catch.
    if VERITY_REQUIRED.load(Ordering::Relaxed) {
        let verified = matches!(
            outcome,
            DiskAttachOutcome::NoDisk
                | DiskAttachOutcome::Mounted(slopos_fs::Ext2MountInfo {
                    verity: VerityStatus::Verified { .. },
                    ..
                })
                | DiskAttachOutcome::Mounted(slopos_fs::Ext2MountInfo {
                    verity: VerityStatus::VerifiedWritable { .. },
                    ..
                })
        );
        if !verified {
            klog_info!(
                "FS: verity=require but the root disk is not verified: {:?}",
                outcome
            );
            return -1;
        }
    }

    if ROOTFS_IS_RAMFS.load(Ordering::Relaxed) {
        if let Some(fs) = root_ext2() {
            let flags = ext2_mount_flags(fs);
            match mount(b"/mnt", fs, flags) {
                Ok(_) => klog_info!(
                    "VFS: mounted ext2 at /mnt (secondary, {})",
                    if flags & MOUNT_RDONLY != 0 {
                        "read-only"
                    } else {
                        "read-write"
                    },
                ),
                Err(e) => klog_info!("VFS: failed to mount ext2 at /mnt: {:?}", e),
            }
        }
        return 0;
    }

    let root = root_ext2().map_or(RootBacking::Ramfs, RootBacking::Ext2);
    if vfs_init_builtin_filesystems_with(root).is_ok() {
        if let Some(fs) = root_ext2() {
            // The kernel-test phase may already have mounted RamFs at `/` and
            // tripped the one-shot init flag, so the call above returned without
            // mounting ext2.
            let _ = unmount(b"/");
            let flags = ext2_mount_flags(fs);
            match mount(b"/", fs, flags) {
                Ok(_) => {
                    if flags & MOUNT_RDONLY == 0 {
                        slopos_ostd::boot_flags::set_flag(
                            slopos_ostd::boot_flags::BOOT_FLAG_ROOT_PERSISTENT,
                        );
                    }
                    klog_info!(
                        "VFS: mounted / (ext2, {}), /tmp (ramfs), /dev (devfs), /dev/shm (ramfs)",
                        if flags & MOUNT_RDONLY != 0 {
                            "read-only"
                        } else {
                            "read-write"
                        },
                    );
                    if BASE_OVER_DISK.load(Ordering::Relaxed) && mount_base() != 0 {
                        return -1;
                    }
                }
                Err(e) => {
                    klog_info!("VFS: failed to install ext2 root: {:?}", e);
                    return -1;
                }
            }
        } else {
            klog_info!("VFS: mounted /tmp (ramfs), /dev (devfs), /dev/shm (ramfs)");
        }
    } else {
        klog_info!("VFS: failed to mount builtin filesystems");
        return -1;
    }

    0
}

/// `mount=<source>:<path>`, repeatable and applied in cmdline order, so a later
/// entry may mount inside an earlier one. Each is an ext2 mount through the
/// same path `mount(2)` takes; a failure is one line and the boot goes on, as
/// an absent `root=` device does.
fn boot_step_cmdline_mounts_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    let Some(cmdline) =
        slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(crate::early_init::boot_get_cmdline())
    else {
        return;
    };
    for token in cmdline.split_ascii_whitespace() {
        if let Some(spec) = token.strip_prefix("mount=") {
            apply_cmdline_mount(spec);
        }
    }
}

#[inline(never)]
fn apply_cmdline_mount(spec: &str) {
    let Some((source, path)) = crate::early_init::parse_mount_option(spec) else {
        klog_info!(
            "MOUNT: mount={} ignored (want <device>|LABEL=<label>:/<path>)",
            spec
        );
        return;
    };
    match slopos_core::syscall::fs::mount_handlers::mount_apply_at(
        source.as_bytes(),
        path.as_bytes(),
        b"/",
        b"ext2",
        0,
    ) {
        Ok(()) => {
            let read_only = slopos_fs::vfs::canon::canonicalise(path.as_bytes())
                .ok()
                .and_then(|canon| slopos_fs::vfs::mount_at(canon.as_bytes()))
                .is_some_and(|m| m.read_only());
            klog_info!(
                "MOUNT: {} at {} (ext2, {})",
                source,
                path,
                if read_only { "read-only" } else { "read-write" }
            );
        }
        Err(e) => klog_info!("MOUNT: mount={} failed ({:?}); continuing", spec, e),
    }
}

/// Serve the install medium when the loader carried one: the `install`
/// module's archive, with the kernel and base it booted at the paths a slot
/// holds them under, read-only at [`layout::MEDIUM_DIR`]. The loader keeps
/// all three mapped, so nothing is copied.
fn boot_step_install_medium_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    let Some(archive) = crate::limine_protocol::module(layout::MEDIUM_MODULE) else {
        return;
    };
    let (Some(kernel), Some(base)) = (
        crate::limine_protocol::kernel_file(),
        crate::limine_protocol::initramfs(),
    ) else {
        klog_info!("INSTALL: the medium came without a kernel file or a base; not served");
        return;
    };
    let medium = &slopos_fs::basefs::MEDIUM_FS;
    let files = [
        (layout::MEDIUM_KERNEL.as_bytes(), kernel),
        (layout::MEDIUM_BASE.as_bytes(), base),
    ];
    let entries = match medium.install(archive, &files) {
        Ok(entries) => entries,
        Err(e) => {
            klog_info!("INSTALL: the install module is no medium: {:?}", e);
            return;
        }
    };
    match slopos_fs::vfs::init::vfs_mount_readonly(layout::MEDIUM_DIR.as_bytes(), medium) {
        Ok(()) => klog_info!(
            "INSTALL: the medium is at {} ({} entries, {} bytes)",
            layout::MEDIUM_DIR,
            entries,
            archive.len()
        ),
        Err(e) => klog_info!(
            "INSTALL: the medium could not be mounted at {}: {:?}",
            layout::MEDIUM_DIR,
            e
        ),
    }
}

fn boot_step_init_launch(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    match exec::launch_init() {
        Ok(task_id) => {
            klog_info!("USERLAND: launched /sbin/init as task {}", task_id);
            0
        }
        Err(err) => {
            klog_info!("USERLAND: failed to launch /sbin/init ({:?})", err);
            -1
        }
    }
}

crate::boot_init!(
    BOOT_STEP_TASK_MANAGER,
    services,
    b"task manager\0",
    boot_step_task_manager_init,
    fallible,
    flags = boot_init_priority(20)
);
crate::boot_init!(
    BOOT_STEP_SCHEDULER,
    services,
    b"scheduler\0",
    boot_step_scheduler_init,
    fallible,
    flags = boot_init_priority(30)
);
crate::boot_init!(
    BOOT_STEP_IDLE_TASK,
    services,
    b"idle task\0",
    boot_step_idle_task,
    fallible,
    flags = boot_init_priority(50)
);
crate::boot_init!(
    BOOT_STEP_ROOTFS_INIT,
    services,
    b"initramfs root\0",
    boot_step_rootfs_init,
    fallible,
    flags = boot_init_priority(54)
);
crate::boot_init!(
    BOOT_STEP_FS_INIT,
    services,
    b"fs init\0",
    boot_step_fs_init,
    fallible,
    flags = boot_init_priority(55)
);
crate::boot_init!(
    BOOT_STEP_CMDLINE_MOUNTS,
    services,
    b"cmdline mounts\0",
    boot_step_cmdline_mounts_fn,
    flags = boot_init_priority(56)
);
crate::boot_init!(
    BOOT_STEP_INSTALL_MEDIUM,
    services,
    b"install medium\0",
    boot_step_install_medium_fn,
    flags = boot_init_priority(57)
);
crate::boot_init!(
    BOOT_STEP_INIT_LAUNCH,
    services,
    b"launch /sbin/init\0",
    boot_step_init_launch,
    fallible,
    flags = boot_init_priority(58)
);

fn boot_step_mark_kernel_ready_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    boot_mark_initialized();
    klog_info!("Kernel core services initialized.");
}

crate::boot_init!(
    BOOT_STEP_MARK_READY,
    services,
    b"mark ready\0",
    boot_step_mark_kernel_ready_fn,
    flags = boot_init_priority(60)
);
