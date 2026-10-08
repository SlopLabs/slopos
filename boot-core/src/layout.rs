//! The disk SlopOS boots from, wherever it lives beside another system:
//!
//! | Partition    | Holds                                                   |
//! |--------------|---------------------------------------------------------|
//! | ESP          | Limine at [`LOADER`] and its configuration at [`LOADER_CONFIG`] |
//! | SlopOS boot  | FAT32: each slot's kernel and base, under `/boot/<slot>/` |
//! | SlopOS root  | ext4: `/`                                               |
//! | SlopOS crash | raw: the last panic                                     |
//!
//! An install medium carries one more, the payload: an ext4 volume holding
//! the toolchain and a clone of the source, read-only to every system.
//!
//! SlopOS's partitions carry type GUIDs of its own. The Discoverable
//! Partitions Specification's root and XBOOTLDR types would have a Linux on
//! the same disk mount them as its own, and write its kernels into the boot
//! partition.

use crate::guid::Guid;

const MIB: u64 = 1 << 20;

pub const ESP_TYPE: Guid = Guid::from_spelling("c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
pub const BOOT_TYPE: Guid = Guid::from_spelling("0a5d2380-494f-4bb7-9fa1-76e03c03d1ec");
pub const ROOT_TYPE: Guid = Guid::from_spelling("dc5e4e29-da9a-4e3e-ac44-38b4ea426284");
pub const CRASH_TYPE: Guid = Guid::from_spelling("2f690270-a513-45e2-8e3b-75aa8e44177c");
pub const PAYLOAD_TYPE: Guid = Guid::from_spelling("2af194ea-b926-4003-88fe-08d832269dd9");

pub const ESP_NAME: &str = "EFI system partition";
pub const BOOT_NAME: &str = "SlopOS boot";
pub const ROOT_NAME: &str = "SlopOS root";
pub const CRASH_NAME: &str = "SlopOS crash";

pub const ESP_LABEL: &str = "SLOPOS-ESP";
pub const BOOT_LABEL: &str = "SLOPOS-BOOT";

/// The boundary every partition SlopOS creates starts and ends on.
pub const ALIGN_BYTES: u64 = MIB;
/// An ESP SlopOS creates: room for FAT32's least 65525 clusters at 4 KiB
/// beside their two FATs, so the same size formats on a 4K-native disk.
pub const NEW_ESP_BYTES: u64 = 260 * MIB;
/// Two slots of the largest system, the tests kernel and base, with room.
pub const BOOT_BYTES: u64 = 1024 * MIB;
pub const CRASH_BYTES: u64 = 4 * MIB;
/// The largest root the kernel mounts: its block numbers are 32 bits wide,
/// and the profile's blocks are 4 KiB.
pub const ROOT_MAX_BYTES: u64 = (1 << 44) - ALIGN_BYTES;

/// UEFI paths on the ESP. Everything SlopOS writes to a shared ESP is under
/// [`LOADER_DIR`].
pub const LOADER_DIR: &str = r"\EFI\SlopOS";
pub const LOADER: &str = r"\EFI\SlopOS\BOOTX64.EFI";
/// Limine reads the configuration beside its own image first, so this one
/// is found ahead of any other loader's on the same ESP.
pub const LOADER_CONFIG: &str = r"\EFI\SlopOS\limine.conf";
/// Limine's licence, and the notices of the third-party code built into its
/// binaries, which upstream requires to accompany them.
pub const LOADER_LICENSE: &str = r"\EFI\SlopOS\LICENSE.limine";
pub const LOADER_NOTICES: &str = r"\EFI\SlopOS\3RDPARTY.limine.md";
/// The removable-media path, written only on an ESP SlopOS created: on a
/// shared one it belongs to whoever put a loader there.
pub const FALLBACK_LOADER: &str = r"\EFI\BOOT\BOOTX64.EFI";
pub const FALLBACK_CONFIG: &str = r"\EFI\BOOT\limine.conf";

/// Where the live system serves its install medium: the archive the loader
/// carries as the module [`MEDIUM_MODULE`], beside the kernel and base it
/// booted, which an install copies into every slot.
pub const MEDIUM_DIR: &str = "/media/install";
pub const MEDIUM_MODULE: &str = "install";
pub const MEDIUM_KERNEL: &str = "boot/kernel.elf";
pub const MEDIUM_BASE: &str = "boot/base.img";
/// Limine, as the archive carries it for an ESP, with its licence and
/// notices.
pub const MEDIUM_LOADER: &str = "boot/BOOTX64.EFI";
pub const MEDIUM_LOADER_LICENSE: &str = "boot/LICENSE.limine";
pub const MEDIUM_LOADER_NOTICES: &str = "boot/3RDPARTY.limine.md";
/// The third-party notices, which go beside the slots.
pub const MEDIUM_NOTICE: &str = "boot/NOTICE.md";
/// The GPT disk GUID the medium's image was built with, spelled out: the
/// disk the installer never offers, however the loader was started.
pub const MEDIUM_DISK_GUID: &str = "boot/disk-guid";

/// Where the live system mounts the medium's payload partition, which holds
/// the toolchain and the clone where an installed root has them.
pub const PAYLOAD_DIR: &str = "/media/payload";

/// The description of SlopOS's firmware boot entry.
pub const FIRMWARE_ENTRY: &str = "SlopOS";

/// The slots a disk is laid out with.
pub const SLOTS: [&str; 2] = ["a", "b"];
/// A slot's Limine entry is `slopos-<slot>`.
pub const ENTRY_PREFIX: &str = "slopos-";
/// On the boot partition, a slot is `<SLOTS_DIR>/<slot>/`, holding these two.
pub const SLOTS_DIR: &str = "/boot";
pub const KERNEL_FILE: &str = "kernel.elf";
pub const BASE_FILE: &str = "base.img";
pub const SLOT_NAME_MAX: usize = 8;

/// One to eight lowercase letters or digits: a FAT 8.3 directory name, which
/// is all the boot partition's writer creates.
pub fn valid_slot(slot: &str) -> bool {
    (1..=SLOT_NAME_MAX).contains(&slot.len())
        && slot
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub fn slot_of_kernel(path: &str) -> Option<&str> {
    let slot = path
        .strip_prefix(SLOTS_DIR)?
        .strip_prefix('/')?
        .strip_suffix(KERNEL_FILE)?
        .strip_suffix('/')?;
    valid_slot(slot).then_some(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_types_are_distinct_and_none_is_linux_s() {
        let xbootldr = Guid::from_spelling("bc13c2ff-59e6-4262-a352-b275fd6f7172");
        let linux_root = Guid::from_spelling("4f68bce3-e8cd-4db1-96e7-fbcaf984b709");
        let ours = [BOOT_TYPE, ROOT_TYPE, CRASH_TYPE, PAYLOAD_TYPE];
        for (i, a) in ours.iter().enumerate() {
            assert!(*a != xbootldr && *a != linux_root && *a != ESP_TYPE);
            assert!(ours[i + 1..].iter().all(|b| a != b));
        }
    }

    #[test]
    fn sizes_are_whole_alignment_units() {
        for size in [NEW_ESP_BYTES, BOOT_BYTES, CRASH_BYTES] {
            assert_eq!(size % ALIGN_BYTES, 0);
        }
        let clusters = 65_525 * 4096;
        let fats = 2 * (65_525 + 2) * 4;
        let reserved = 32 * 4096;
        assert!(NEW_ESP_BYTES >= clusters + fats + reserved);
    }

    #[test]
    fn slots_are_short_lowercase_names() {
        assert!(valid_slot("a") && valid_slot("b2") && valid_slot("abcdefgh"));
        assert!(
            !valid_slot("") && !valid_slot("A") && !valid_slot("abcdefghi") && !valid_slot("a-b")
        );
        assert!(SLOTS.iter().all(|slot| valid_slot(slot)));
    }

    #[test]
    fn a_slot_kernel_names_its_slot() {
        assert_eq!(slot_of_kernel("/boot/b/kernel.elf"), Some("b"));
        assert_eq!(slot_of_kernel("/boot/bad/kernel.elf"), Some("bad"));
        for other in [
            "/boot/kernel.elf",
            "/boot/B/kernel.elf",
            "/boot/a/b/kernel.elf",
            "/boot/a/base.img",
            "boot/a/kernel.elf",
        ] {
            assert_eq!(slot_of_kernel(other), None, "{other}");
        }
    }

    #[test]
    fn everything_on_the_esp_is_under_the_vendor_directory_but_the_fallback() {
        for path in [LOADER, LOADER_CONFIG, LOADER_LICENSE, LOADER_NOTICES] {
            assert!(path.starts_with(LOADER_DIR) && path.as_bytes()[LOADER_DIR.len()] == b'\\');
        }
    }
}
