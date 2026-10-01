//! The feature profile SlopOS formats its own volumes with, read from the
//! `profile` file beside this crate so the host's image builder and the
//! programs compiled here agree on one text.

use crate::superblock::{compat, incompat, ro_compat};

pub const TEXT: &str = include_str!("../profile");

pub fn value(key: &str) -> Option<&'static str> {
    TEXT.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// `mke2fs -O`'s comma-separated feature list.
pub fn features() -> &'static str {
    value("features").unwrap_or("")
}

pub fn inode_size() -> u16 {
    value("inode_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

pub fn block_size() -> u32 {
    value("block_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096)
}

/// Which feature word a name sets, and the bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Compat(u32),
    Incompat(u32),
    RoCompat(u32),
}

/// The feature bit `mke2fs` calls `name`.
pub fn feature(name: &str) -> Option<Feature> {
    Some(match name {
        "has_journal" => Feature::Compat(compat::HAS_JOURNAL),
        "ext_attr" => Feature::Compat(compat::EXT_ATTR),
        "resize_inode" => Feature::Compat(compat::RESIZE_INODE),
        "dir_index" => Feature::Compat(compat::DIR_INDEX),
        "orphan_file" => Feature::Compat(compat::ORPHAN_FILE),
        "fast_commit" => Feature::Compat(compat::FAST_COMMIT),
        "filetype" => Feature::Incompat(incompat::FILETYPE),
        "extent" | "extents" => Feature::Incompat(incompat::EXTENTS),
        "64bit" => Feature::Incompat(incompat::BIT64),
        "flex_bg" => Feature::Incompat(incompat::FLEX_BG),
        "meta_bg" => Feature::Incompat(incompat::META_BG),
        "metadata_csum_seed" => Feature::Incompat(incompat::CSUM_SEED),
        "inline_data" => Feature::Incompat(incompat::INLINE_DATA),
        "large_dir" => Feature::Incompat(incompat::LARGEDIR),
        "encrypt" => Feature::Incompat(incompat::ENCRYPT),
        "casefold" => Feature::Incompat(incompat::CASEFOLD),
        "sparse_super" => Feature::RoCompat(ro_compat::SPARSE_SUPER),
        "large_file" => Feature::RoCompat(ro_compat::LARGE_FILE),
        "huge_file" => Feature::RoCompat(ro_compat::HUGE_FILE),
        "uninit_bg" => Feature::RoCompat(ro_compat::GDT_CSUM),
        "dir_nlink" => Feature::RoCompat(ro_compat::DIR_NLINK),
        "extra_isize" => Feature::RoCompat(ro_compat::EXTRA_ISIZE),
        "quota" => Feature::RoCompat(ro_compat::QUOTA),
        "bigalloc" => Feature::RoCompat(ro_compat::BIGALLOC),
        "metadata_csum" => Feature::RoCompat(ro_compat::METADATA_CSUM),
        "project" => Feature::RoCompat(ro_compat::PROJECT),
        _ => return None,
    })
}

/// The profile's features as the `(compat, incompat, ro_compat)` words, or
/// the first name that is not a feature.
pub fn feature_words() -> Result<(u32, u32, u32), &'static str> {
    let (mut c, mut i, mut r) = (0, 0, 0);
    for name in features().split(',').filter(|n| !n.is_empty()) {
        match feature(name).ok_or(name)? {
            Feature::Compat(bit) => c |= bit,
            Feature::Incompat(bit) => i |= bit,
            Feature::RoCompat(bit) => r |= bit,
        }
    }
    Ok((c, i, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::{WRITABLE_COMPAT, WRITABLE_INCOMPAT, WRITABLE_RO_COMPAT};

    #[test]
    fn every_profile_feature_is_one_the_kernel_writes() {
        let (c, i, r) = feature_words().expect("profile names a feature mke2fs does not");
        assert_eq!(c & !WRITABLE_COMPAT, 0);
        assert_eq!(i & !WRITABLE_INCOMPAT, 0);
        assert_eq!(r & !WRITABLE_RO_COMPAT, 0);
        assert_ne!(c & compat::HAS_JOURNAL, 0);
        assert_ne!(r & ro_compat::METADATA_CSUM, 0);
    }

    #[test]
    fn values() {
        assert_eq!(inode_size(), 256);
        assert_eq!(block_size(), 4096);
        assert!(features().contains("extent"));
    }
}
