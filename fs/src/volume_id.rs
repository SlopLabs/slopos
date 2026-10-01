//! A volume's own identity: the UUID and label its filesystem records, which
//! `UUID=`, `LABEL=`, `/dev/disk/by-uuid` and `/dev/disk/by-label` name it by.
//!
//! Read from the formats' published layouts: ext2/3/4's superblock at byte
//! 1024, btrfs's at 64 KiB, and the FAT boot sector's extended BPB, whose
//! label the root directory's volume-label entry overrides as `fatlabel`
//! writes both.

use slopos_ostd::KVec;

use crate::blockdev::BlockDevice;

pub const UUID_TEXT_MAX: usize = 36;
/// Longer labels exist only on btrfs, and go unnamed rather than truncated.
pub const LABEL_MAX: usize = 64;

const HEAD_BYTES: usize = 4096;
const EXT_SUPERBLOCK: usize = 1024;
const EXT_MAGIC: u16 = 0xEF53;
const BTRFS_SUPERBLOCK: u64 = 64 * 1024;
const BTRFS_MAGIC: &[u8; 8] = b"_BHRfS_M";
const FAT_NO_NAME: &[u8; 11] = b"NO NAME    ";
const FAT_EXTENDED_BOOT_SIGNATURE: u8 = 0x29;
const FAT_ATTR_VOLUME_ID: u8 = 0x08;
const FAT_ATTR_DIRECTORY: u8 = 0x10;
const FAT_ATTR_LONG_NAME: u8 = 0x0F;
const FAT_DIRENT_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    Ext,
    Btrfs,
    Vfat,
}

#[derive(Clone, Copy)]
pub struct VolumeId {
    pub kind: VolumeKind,
    uuid: [u8; UUID_TEXT_MAX],
    uuid_len: u8,
    label: [u8; LABEL_MAX],
    label_len: u8,
}

impl VolumeId {
    fn new(kind: VolumeKind) -> Self {
        Self {
            kind,
            uuid: [0; UUID_TEXT_MAX],
            uuid_len: 0,
            label: [0; LABEL_MAX],
            label_len: 0,
        }
    }

    pub fn uuid(&self) -> Option<&[u8]> {
        (self.uuid_len > 0).then(|| &self.uuid[..usize::from(self.uuid_len)])
    }

    pub fn label(&self) -> Option<&[u8]> {
        (self.label_len > 0).then(|| &self.label[..usize::from(self.label_len)])
    }

    fn set_label(&mut self, label: &[u8]) {
        if label.len() <= LABEL_MAX {
            self.label[..label.len()].copy_from_slice(label);
            self.label_len = label.len() as u8;
        }
    }

    fn set_uuid(&mut self, text: &[u8]) {
        self.uuid[..text.len()].copy_from_slice(text);
        self.uuid_len = text.len() as u8;
    }

    /// A UUID stored as sixteen bytes in text order, as ext4 and btrfs keep
    /// theirs. All zeros is none.
    fn set_uuid_bytes(&mut self, bytes: &[u8]) {
        if bytes.iter().all(|&b| b == 0) {
            return;
        }
        let mut text = [0u8; UUID_TEXT_MAX];
        let mut at = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                text[at] = b'-';
                at += 1;
            }
            text[at] = HEX_LOWER[usize::from(byte >> 4)];
            text[at + 1] = HEX_LOWER[usize::from(byte & 0xF)];
            at += 2;
        }
        self.set_uuid(&text);
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn until_nul(field: &[u8]) -> &[u8] {
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..len]
}

fn trim_spaces(field: &[u8]) -> &[u8] {
    let len = field
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    &field[..len]
}

/// The device could not be read, or no memory could be had to read it into:
/// nothing is known of its volume, which a later probe may yet name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable;

/// `len` bytes at `offset`, or `None` when the device ends first.
fn read_staged(
    device: &dyn BlockDevice,
    offset: u64,
    len: usize,
) -> Result<Option<KVec<u8>>, Unreadable> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= device.capacity() => {}
        _ => return Ok(None),
    }
    let mut buf = KVec::zeroed(len).map_err(|_| Unreadable)?;
    device.read_at(offset, &mut buf).map_err(|_| Unreadable)?;
    Ok(Some(buf))
}

/// The filesystem on `device`, if it is one this reads, with whatever UUID
/// and label it records.
pub fn probe(device: &dyn BlockDevice) -> Result<Option<VolumeId>, Unreadable> {
    let Some(head) = read_staged(device, 0, HEAD_BYTES)? else {
        return Ok(None);
    };
    if let Some(id) = probe_ext(&head) {
        return Ok(Some(id));
    }
    // A btrfs superblock that cannot be read leaves the volume unknown only
    // if the head names no FAT volume either.
    let btrfs = probe_btrfs(device);
    if let Ok(Some(id)) = btrfs {
        return Ok(Some(id));
    }
    match probe_vfat(device, &head)? {
        Some(id) => Ok(Some(id)),
        None => btrfs,
    }
}

fn probe_ext(head: &[u8]) -> Option<VolumeId> {
    let sb = &head[EXT_SUPERBLOCK..];
    if le16(sb, 56) != EXT_MAGIC || le32(sb, 76) < 1 {
        return None;
    }
    let mut id = VolumeId::new(VolumeKind::Ext);
    id.set_uuid_bytes(&sb[104..120]);
    id.set_label(until_nul(&sb[120..136]));
    Some(id)
}

fn probe_btrfs(device: &dyn BlockDevice) -> Result<Option<VolumeId>, Unreadable> {
    let Some(sb) = read_staged(device, BTRFS_SUPERBLOCK, HEAD_BYTES)? else {
        return Ok(None);
    };
    if &sb[0x40..0x48] != BTRFS_MAGIC {
        return Ok(None);
    }
    let mut id = VolumeId::new(VolumeKind::Btrfs);
    id.set_uuid_bytes(&sb[0x20..0x30]);
    id.set_label(until_nul(&sb[0x12B..0x22B]));
    Ok(Some(id))
}

/// The BIOS parameter block fields that place the root directory.
struct FatLayout {
    bytes_per_sector: u64,
    root_dir: u64,
    root_dir_bytes: usize,
    /// Offset of the extended BPB: 36 on FAT12/16, 64 on FAT32.
    ext_bpb: usize,
}

fn fat_layout(bs: &[u8]) -> Option<FatLayout> {
    if le16(bs, 510) != 0xAA55 {
        return None;
    }
    let bps = le16(bs, 11);
    let spc = bs[13];
    let reserved = u64::from(le16(bs, 14));
    let fats = u64::from(bs[16]);
    let root_entries = u64::from(le16(bs, 17));
    let media = bs[21];
    let fat16_size = u64::from(le16(bs, 22));
    let sane = matches!(bps, 512 | 1024 | 2048 | 4096)
        && spc.is_power_of_two()
        && reserved > 0
        && fats > 0
        && (media == 0xF0 || media >= 0xF8);
    if !sane {
        return None;
    }
    let bps = u64::from(bps);
    let cluster = bps * u64::from(spc);
    if fat16_size == 0 {
        if &bs[82..87] != b"FAT32" {
            return None;
        }
        let data = reserved + fats * u64::from(le32(bs, 36));
        let root_cluster = u64::from(le32(bs, 44)).checked_sub(2)?;
        Some(FatLayout {
            bytes_per_sector: bps,
            root_dir: (data + root_cluster * u64::from(spc)) * bps,
            root_dir_bytes: cluster.min(HEAD_BYTES as u64) as usize,
            ext_bpb: 64,
        })
    } else {
        if &bs[54..57] != b"FAT" {
            return None;
        }
        let root_bytes = root_entries * FAT_DIRENT_BYTES as u64;
        Some(FatLayout {
            bytes_per_sector: bps,
            root_dir: (reserved + fats * fat16_size) * bps,
            root_dir_bytes: root_bytes.min(HEAD_BYTES as u64) as usize,
            ext_bpb: 36,
        })
    }
}

fn probe_vfat(device: &dyn BlockDevice, head: &[u8]) -> Result<Option<VolumeId>, Unreadable> {
    let Some(layout) = fat_layout(head) else {
        return Ok(None);
    };
    if layout.bytes_per_sector < u64::from(device.logical_block_size()) {
        return Ok(None);
    }
    let mut id = VolumeId::new(VolumeKind::Vfat);
    let ext = layout.ext_bpb;
    if head[ext + 2] == FAT_EXTENDED_BOOT_SIGNATURE {
        let serial = le32(head, ext + 3).to_be_bytes();
        let mut text = [0u8; 9];
        let mut at = 0;
        for (i, byte) in serial.into_iter().enumerate() {
            if i == 2 {
                text[at] = b'-';
                at += 1;
            }
            text[at] = HEX_UPPER[usize::from(byte >> 4)];
            text[at + 1] = HEX_UPPER[usize::from(byte & 0xF)];
            at += 2;
        }
        id.set_uuid(&text);
        let label = &head[ext + 7..ext + 18];
        if label != FAT_NO_NAME {
            id.set_label(trim_spaces(label));
        }
    }
    if let Some(root) = read_staged(device, layout.root_dir, layout.root_dir_bytes)?
        && let Some(label) = root_dir_label(&root)
    {
        id.set_label(label);
    }
    Ok(Some(id))
}

/// The volume-label entry among a root directory's first entries.
fn root_dir_label(dir: &[u8]) -> Option<&[u8]> {
    for entry in dir.chunks_exact(FAT_DIRENT_BYTES) {
        match entry[0] {
            0x00 => return None,
            0xE5 => continue,
            _ => {}
        }
        let attr = entry[11];
        if attr == FAT_ATTR_LONG_NAME || attr & FAT_ATTR_DIRECTORY != 0 {
            continue;
        }
        if attr & FAT_ATTR_VOLUME_ID != 0 {
            let label = trim_spaces(&entry[..11]);
            return (!label.is_empty()).then_some(label);
        }
    }
    None
}
