//! The install medium the live system serves: what the loader brought at
//! [`layout::MEDIUM_DIR`] — the kernel every slot gets, the base every slot
//! boots and Limine for the ESP — and on a stick built with the payload, the
//! partition the kernel mounts at [`layout::PAYLOAD_DIR`], the toolchain for
//! `/usr/local` and a clone of the source for `/src`.

use std::fs;
use std::path::PathBuf;

use slopos_boot_core::Guid;
use slopos_boot_core::layout;

use super::root;
use crate::boot_disk::{disk_guid_of, medium_disk_guid, table_of, whole_disks};

/// What a root holds without the payload: the system's own writes.
const ROOT_MIN_BYTES: u64 = 1 << 30;
/// The room a root keeps beside the payload for building the system, which a
/// clean dev and tests build holds 3.8 GiB of.
const ROOT_WORK_BYTES: u64 = 6 << 30;

pub struct Medium {
    /// The GPT disk GUID its image was built with: the disk never offered.
    pub disk_guid: Guid,
    payload: bool,
    /// Whether the payload carries the toolchain for `/usr/local`.
    pub toolchain: bool,
    /// Whether the payload carries the clone for `/src`.
    pub source: bool,
    /// The disk whose table names the medium's payload when the kernel did
    /// not mount it.
    pub unmounted_payload: Option<String>,
    /// The root's least size: the payload and room to build beside it, or the
    /// system's own needs without one.
    pub root_min: u64,
}

/// The directory `dir`, not a link to one, and the filesystem it is on.
fn dir_and_fs(dir: &str) -> Option<(fs::Metadata, slopos_abi::fs::UserStatfs)> {
    let meta = fs::symlink_metadata(dir)
        .ok()
        .filter(|meta| meta.is_dir())?;
    let path = std::ffi::CString::new(dir).ok()?;
    let stats = crate::syscall::fs::statfs_path(path.as_ptr()).ok()?;
    Some((meta, stats))
}

/// Whether the kernel mounted the medium's payload: the root of a read-only
/// ext4 volume at [`layout::PAYLOAD_DIR`], which no program fakes with a
/// directory.
fn payload_mounted() -> bool {
    dir_and_fs(layout::PAYLOAD_DIR).is_some_and(|(meta, stats)| {
        std::os::unix::fs::MetadataExt::ino(&meta) == root::EXT4_ROOT_INODE
            && stats.f_type == slopos_abi::fs::EXT2_SUPER_MAGIC
            && stats.f_flags & slopos_abi::fs::ST_RDONLY != 0
    })
}

/// The disk carrying the medium's GPT disk GUID, when its table names a
/// payload partition.
fn unmounted_payload(disk_guid: Guid) -> Option<String> {
    whole_disks().ok()?.into_iter().find(|node| {
        disk_guid_of(node) == Some(disk_guid)
            && table_of(node).is_some_and(|table| {
                table
                    .partitions()
                    .any(|p| p.entry.type_guid == layout::PAYLOAD_TYPE)
            })
    })
}

impl Medium {
    /// The medium, whole: what every install takes, and what a payload's
    /// trees take with them, checked before anything is written.
    pub fn find() -> Result<Medium, String> {
        let dir = layout::MEDIUM_DIR;
        let served =
            dir_and_fs(dir).is_some_and(|(_, stats)| stats.f_type == slopos_abi::fs::BASEFS_MAGIC);
        if !served {
            return Err(format!(
                "no install medium at {dir}; boot the SlopOS ISO to install"
            ));
        }
        let disk_guid = medium_disk_guid()?.ok_or_else(|| {
            format!(
                "no install medium: {dir}/{} is missing; boot the SlopOS ISO to install",
                layout::MEDIUM_DISK_GUID
            )
        })?;
        let payload = payload_mounted();
        let mut medium = Medium {
            disk_guid,
            payload,
            toolchain: false,
            source: false,
            unmounted_payload: None,
            root_min: ROOT_MIN_BYTES,
        };
        for file in [
            layout::MEDIUM_KERNEL,
            layout::MEDIUM_BASE,
            layout::MEDIUM_LOADER,
            layout::MEDIUM_LOADER_LICENSE,
            layout::MEDIUM_LOADER_NOTICES,
            layout::MEDIUM_APPS,
        ] {
            if !medium.has(file) {
                return Err(format!(
                    "no install medium: {} is missing; boot the SlopOS ISO to install",
                    medium.path(file).display()
                ));
            }
        }
        let toolchain_manifest = slopos_tree_core::manifest_path(slopos_tree_core::TOOLCHAIN);
        for (tree, needs) in [
            (root::LOCAL, toolchain_manifest.as_str()),
            (root::SOURCE, "/src/slopos/.git/config"),
        ] {
            if medium.payload_has(tree) && !medium.payload_has(needs) {
                return Err(format!(
                    "the payload carries {tree} without {needs}; it was not built whole"
                ));
            }
        }
        medium.toolchain = medium.payload_has(root::LOCAL);
        medium.source = medium.payload_has(root::SOURCE);
        if !payload {
            medium.unmounted_payload = unmounted_payload(disk_guid);
        }
        if medium.toolchain || medium.source {
            let path =
                std::ffi::CString::new(layout::PAYLOAD_DIR).map_err(|_| "a NUL in a path")?;
            let stats = crate::syscall::fs::statfs_path(path.as_ptr())
                .map_err(|e| format!("{}: {e:?}", layout::PAYLOAD_DIR))?;
            medium.root_min = stats.f_blocks * stats.f_bsize + ROOT_WORK_BYTES;
        }
        Ok(medium)
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        PathBuf::from(layout::MEDIUM_DIR).join(rel.trim_start_matches('/'))
    }

    pub fn has(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn read(&self, rel: &str) -> Result<Vec<u8>, String> {
        fs::read(self.path(rel)).map_err(|e| format!("{}: {e}", self.path(rel).display()))
    }

    pub fn payload_path(&self, rel: &str) -> PathBuf {
        PathBuf::from(layout::PAYLOAD_DIR).join(rel.trim_start_matches('/'))
    }

    fn payload_has(&self, rel: &str) -> bool {
        self.payload && self.payload_path(rel).exists()
    }
}
