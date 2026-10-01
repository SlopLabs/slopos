//! UEFI device paths, UEFI Specification 2.10 §10.3: a run of nodes, each a
//! type, a subtype and a little-endian length that counts its own four-byte
//! header, closed by an End of Entire Device Path node. A load option names
//! its loader with one: a hard-drive node for the partition, a file-path node
//! for the file on it.

use crate::guid::Guid;

pub const TYPE_MEDIA: u8 = 0x04;
pub const TYPE_END: u8 = 0x7F;
pub const SUBTYPE_HARD_DRIVE: u8 = 0x01;
pub const SUBTYPE_FILE_PATH: u8 = 0x04;
pub const SUBTYPE_END_INSTANCE: u8 = 0x01;
pub const SUBTYPE_END_ENTIRE: u8 = 0xFF;

const NODE_HEADER: usize = 4;
pub const HARD_DRIVE_LEN: usize = 42;
const END_LEN: usize = 4;
const PARTITION_FORMAT_GPT: u8 = 0x02;
const SIGNATURE_TYPE_GUID: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// A node shorter than its header, or reaching past the path.
    BadNode,
    /// No End of Entire Device Path node, or bytes after it.
    Unterminated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    pub kind: u8,
    pub subtype: u8,
    /// The bytes after the four-byte header.
    pub data: &'a [u8],
}

/// The nodes of the first device path in `path`, or of its first instance,
/// before the end node closing it, each with where it ends.
fn walk(path: &[u8]) -> impl Iterator<Item = (usize, Node<'_>)> {
    let mut at = 0;
    core::iter::from_fn(move || {
        let rest = path.get(at..)?;
        let len = usize::from(u16::from_le_bytes([*rest.get(2)?, *rest.get(3)?]));
        let node = rest.get(..len).filter(|_| len >= NODE_HEADER)?;
        if node[0] == TYPE_END {
            at = path.len();
            return None;
        }
        at += len;
        Some((
            at,
            Node {
                kind: node[0],
                subtype: node[1],
                data: &node[NODE_HEADER..],
            },
        ))
    })
}

/// A packed list of device paths, as a load option carries: every node in
/// bounds, every end node exactly its four-byte header, as firmware requires,
/// and each path closed by an End of Entire Device Path node, the last of them
/// at the end of the list.
pub fn validate(list: &[u8]) -> Result<(), Malformed> {
    let mut at = 0;
    let mut closed = false;
    while at < list.len() {
        let header = list.get(at..at + NODE_HEADER).ok_or(Malformed::BadNode)?;
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = header[0] == TYPE_END;
        if len < NODE_HEADER || at + len > list.len() || (end && len != END_LEN) {
            return Err(Malformed::BadNode);
        }
        closed = end && header[1] == SUBTYPE_END_ENTIRE;
        at += len;
    }
    closed.then_some(()).ok_or(Malformed::Unterminated)
}

/// A GPT partition, as a hard-drive media node names it. Start and size count
/// in the disk's logical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardDrive {
    pub partition_number: u32,
    pub start_lba: u64,
    pub blocks: u64,
    pub partition: Guid,
}

impl HardDrive {
    /// `None` for a node that is no hard drive, or one naming an MBR partition.
    pub fn decode(node: &Node<'_>) -> Option<HardDrive> {
        let d = node.data;
        if node.kind != TYPE_MEDIA
            || node.subtype != SUBTYPE_HARD_DRIVE
            || d.len() != HARD_DRIVE_LEN - NODE_HEADER
            || d[36] != PARTITION_FORMAT_GPT
            || d[37] != SIGNATURE_TYPE_GUID
        {
            return None;
        }
        let u64_at = |at: usize| {
            let mut word = [0u8; 8];
            word.copy_from_slice(&d[at..at + 8]);
            u64::from_le_bytes(word)
        };
        let mut partition = [0u8; 16];
        partition.copy_from_slice(&d[20..36]);
        Some(HardDrive {
            partition_number: u32::from_le_bytes([d[0], d[1], d[2], d[3]]),
            start_lba: u64_at(4),
            blocks: u64_at(12),
            partition: Guid(partition),
        })
    }

    fn encode(&self) -> [u8; HARD_DRIVE_LEN] {
        let mut node = [0u8; HARD_DRIVE_LEN];
        node[0] = TYPE_MEDIA;
        node[1] = SUBTYPE_HARD_DRIVE;
        node[2..4].copy_from_slice(&(HARD_DRIVE_LEN as u16).to_le_bytes());
        node[4..8].copy_from_slice(&self.partition_number.to_le_bytes());
        node[8..16].copy_from_slice(&self.start_lba.to_le_bytes());
        node[16..24].copy_from_slice(&self.blocks.to_le_bytes());
        node[24..40].copy_from_slice(&self.partition.0);
        node[40] = PARTITION_FORMAT_GPT;
        node[41] = SIGNATURE_TYPE_GUID;
        node
    }
}

/// The UCS-2 text a file-path node carries, before its terminator.
fn file_text<'a>(node: &Node<'a>) -> Option<&'a [u8]> {
    if node.kind != TYPE_MEDIA || node.subtype != SUBTYPE_FILE_PATH || node.data.len() % 2 != 0 {
        return None;
    }
    let units = node.data.len() / 2;
    let end = (0..units)
        .find(|&i| node.data[2 * i] == 0 && node.data[2 * i + 1] == 0)
        .unwrap_or(units);
    Some(&node.data[..2 * end])
}

/// The non-empty components of UCS-2 text, split at backslashes.
fn components(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = text;
    core::iter::from_fn(move || {
        loop {
            if rest.is_empty() {
                return None;
            }
            let end = rest
                .chunks_exact(2)
                .position(|unit| unit == [b'\\', 0])
                .map_or(rest.len(), |i| 2 * i);
            let (part, tail) = rest.split_at(end);
            rest = tail.get(2..).unwrap_or(&[]);
            if !part.is_empty() {
                return Some(part);
            }
        }
    })
}

fn component_is(units: &[u8], text: &str) -> bool {
    units.len() == 2 * text.len()
        && units
            .chunks_exact(2)
            .zip(text.bytes())
            .all(|(unit, b)| unit[1] == 0 && unit[0].eq_ignore_ascii_case(&b))
}

/// The file a loader's device path names on its partition: one file-path node
/// or several, which spell one path together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePath<'a> {
    nodes: &'a [u8],
}

impl FilePath<'_> {
    /// Whether this spells `path`, component by component and ignoring ASCII
    /// case: FAT, where a loader lives, matches names without it.
    pub fn is(&self, path: &str) -> bool {
        let mut ours = walk(self.nodes)
            .filter_map(|(_, node)| file_text(&node))
            .flat_map(components);
        let mut theirs = path.split('\\').filter(|c| !c.is_empty());
        loop {
            match (ours.next(), theirs.next()) {
                (None, None) => return true,
                (Some(a), Some(b)) if component_is(a, b) => {}
                _ => return false,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    TooSmall,
    /// Longer than a node's 16-bit length can count.
    TooLong,
    /// A path a file-path node carries as UCS-2 must here be ASCII.
    NotAscii,
}

/// The bytes [`loader`] writes for a file `path` of ASCII characters.
pub const fn loader_len(path: &str) -> usize {
    HARD_DRIVE_LEN + NODE_HEADER + 2 * (path.len() + 1) + END_LEN
}

/// `HD(partition)/path/End`, the device path of a loader on a GPT partition.
/// `path` is ASCII with backslash separators, as UEFI spells a file.
pub fn loader(partition: &HardDrive, path: &str, out: &mut [u8]) -> Result<usize, EncodeError> {
    if !path.is_ascii() {
        return Err(EncodeError::NotAscii);
    }
    let len = loader_len(path);
    let file_len = len - HARD_DRIVE_LEN - END_LEN;
    let file_len_field = u16::try_from(file_len).map_err(|_| EncodeError::TooLong)?;
    let out = out.get_mut(..len).ok_or(EncodeError::TooSmall)?;
    out[..HARD_DRIVE_LEN].copy_from_slice(&partition.encode());
    let file = &mut out[HARD_DRIVE_LEN..HARD_DRIVE_LEN + file_len];
    file[0] = TYPE_MEDIA;
    file[1] = SUBTYPE_FILE_PATH;
    file[2..4].copy_from_slice(&file_len_field.to_le_bytes());
    for (unit, b) in file[NODE_HEADER..].chunks_exact_mut(2).zip(path.bytes()) {
        unit.copy_from_slice(&u16::from(b).to_le_bytes());
    }
    file[file_len - 2..].fill(0);
    out[len - END_LEN..].copy_from_slice(&[TYPE_END, SUBTYPE_END_ENTIRE, END_LEN as u8, 0]);
    Ok(len)
}

/// The partition and file a loader's device path names: its last GPT
/// hard-drive node and the file-path nodes closing the path after it, after
/// whatever spelling of the disk an entry carries. A path naming a device and
/// no file, as a firmware's entry for a removable medium does, names none.
pub fn hard_drive_file(path: &[u8]) -> Option<(HardDrive, FilePath<'_>)> {
    let mut found = None;
    let mut files = true;
    for (end, node) in walk(path) {
        if let Some(partition) = HardDrive::decode(&node) {
            found = Some((partition, end, end));
            files = true;
        } else if let Some((_, _, last)) = found.as_mut() {
            files &= file_text(&node).is_some();
            *last = end;
        }
    }
    let (partition, from, to) = found?;
    if !files || to == from {
        return None;
    }
    Some((
        partition,
        FilePath {
            nodes: path.get(from..to)?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const ESP: HardDrive = HardDrive {
        partition_number: 1,
        start_lba: 2048,
        blocks: 532_480,
        partition: Guid::from_spelling("6d3c2a91-5b0e-4b6f-9a51-2f0c3e4d5a6b"),
    };
    const LOADER: &str = r"\EFI\SlopOS\BOOTX64.EFI";

    fn built() -> Vec<u8> {
        let mut out = std::vec![0u8; loader_len(LOADER)];
        assert_eq!(loader(&ESP, LOADER, &mut out), Ok(out.len()));
        out
    }

    fn node(kind: u8, subtype: u8, data: &[u8]) -> Vec<u8> {
        let mut out = std::vec![kind, subtype];
        out.extend_from_slice(&((data.len() + 4) as u16).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn file(text: &str) -> Vec<u8> {
        let units: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        node(TYPE_MEDIA, SUBTYPE_FILE_PATH, &units)
    }

    const END: [u8; 4] = [TYPE_END, SUBTYPE_END_ENTIRE, 4, 0];

    #[test]
    fn a_loader_path_has_the_spec_layout() {
        let path = built();
        assert_eq!(&path[..4], &[0x04, 0x01, 42, 0]);
        assert_eq!(&path[4..8], &1u32.to_le_bytes());
        assert_eq!(&path[8..16], &2048u64.to_le_bytes());
        assert_eq!(&path[16..24], &532_480u64.to_le_bytes());
        assert_eq!(&path[24..40], &ESP.partition.0);
        assert_eq!(&path[40..42], &[0x02, 0x02]);
        let file_len = 4 + 2 * (LOADER.len() + 1);
        assert_eq!(&path[42..46], &[0x04, 0x04, file_len as u8, 0]);
        assert_eq!(&path[42 + file_len..], &END);
        assert_eq!(path.len(), 98);
        assert_eq!(validate(&path), Ok(()));
    }

    #[test]
    fn a_loader_path_decodes_to_what_built_it() {
        let path = built();
        let (partition, file) = hard_drive_file(&path).unwrap();
        assert_eq!(partition, ESP);
        assert!(file.is(LOADER));
        assert!(file.is(&LOADER.to_ascii_lowercase()));
        assert!(!file.is(r"\EFI\BOOT\BOOTX64.EFI"));
        assert!(!file.is(r"\EFI\SlopOS"));
        assert!(!file.is(r"\EFI\SlopOS\BOOTX64.EFI\x"));
    }

    /// Firmware may store an entry with the disk's full path ahead of the
    /// partition, and a path split across file nodes.
    #[test]
    fn a_hardware_prefix_and_split_file_nodes_name_the_same_loader() {
        let hd = &built()[..HARD_DRIVE_LEN];
        let pci = node(0x01, 0x01, &[0x00, 0x04]);
        let nvme = node(0x03, 0x17, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        for split in [
            [r"\EFI\SlopOS", "BOOTX64.EFI"],
            [r"\EFI\SlopOS\", r"\BOOTX64.EFI"],
            [r"\EFI", r"SlopOS\BOOTX64.EFI"],
        ] {
            let path: Vec<u8> = [pci.clone(), nvme.clone(), hd.to_vec()]
                .into_iter()
                .chain(split.iter().map(|part| file(part)))
                .chain([END.to_vec()])
                .flatten()
                .collect();
            assert_eq!(validate(&path), Ok(()));
            let (partition, file) = hard_drive_file(&path).unwrap();
            assert_eq!(partition, ESP);
            assert!(file.is(LOADER), "{split:?}");
        }
    }

    #[test]
    fn a_list_carries_further_paths_after_the_first() {
        let path = built();
        let mut list = path.clone();
        list.extend_from_slice(&path);
        assert_eq!(validate(&list), Ok(()));
        let (partition, _) = hard_drive_file(&list).unwrap();
        assert_eq!(partition, ESP);
    }

    #[test]
    fn a_path_without_its_end_or_with_a_short_node_is_refused() {
        let path = built();
        assert_eq!(
            validate(&path[..path.len() - 4]),
            Err(Malformed::Unterminated)
        );
        let mut trailing = path.clone();
        trailing.extend_from_slice(&path[..HARD_DRIVE_LEN]);
        assert_eq!(validate(&trailing), Err(Malformed::Unterminated));
        let mut short = path.clone();
        short[2] = 3;
        assert_eq!(validate(&short), Err(Malformed::BadNode));
        let mut long = path;
        long[2] = 0xFF;
        assert_eq!(validate(&long), Err(Malformed::BadNode));
        assert_eq!(validate(&[]), Err(Malformed::Unterminated));
    }

    #[test]
    fn an_end_node_longer_than_its_header_is_refused() {
        let mut path = built();
        let at = path.len() - 4;
        path[at + 2] = 8;
        path.extend_from_slice(&[0; 4]);
        assert_eq!(validate(&path), Err(Malformed::BadNode));
    }

    #[test]
    fn the_first_instance_names_the_loader() {
        let path = built();
        let first = &path[..path.len() - 4];
        let other = HardDrive {
            partition: Guid::from_spelling("00000000-0000-0000-0000-000000000001"),
            ..ESP
        };
        let mut second = std::vec![0u8; loader_len(LOADER)];
        loader(&other, LOADER, &mut second).unwrap();
        let instances: Vec<u8> = [first, &[TYPE_END, SUBTYPE_END_INSTANCE, 4, 0], &second].concat();
        assert_eq!(validate(&instances), Ok(()));
        assert_eq!(hard_drive_file(&instances).unwrap().0, ESP);
    }

    #[test]
    fn a_device_without_a_file_or_with_something_after_it_names_no_loader() {
        let hd = &built()[..HARD_DRIVE_LEN];
        let bare: Vec<u8> = [hd, &END].concat();
        assert_eq!(validate(&bare), Ok(()));
        assert!(hard_drive_file(&bare).is_none());
        let pci = node(0x01, 0x01, &[0x00, 0x04]);
        let odd: Vec<u8> = [hd.to_vec(), file(LOADER), pci, END.to_vec()].concat();
        assert!(hard_drive_file(&odd).is_none());
        let mut mbr = hd.to_vec();
        mbr[40] = 0x01;
        let mbr_path: Vec<u8> = [mbr, file(LOADER), END.to_vec()].concat();
        assert!(hard_drive_file(&mbr_path).is_none());
    }

    #[test]
    fn an_unencodable_path_is_refused_without_writing_past_the_buffer() {
        let mut out = [0u8; 10];
        assert_eq!(loader(&ESP, LOADER, &mut out), Err(EncodeError::TooSmall));
        let mut room = [0u8; 200];
        assert_eq!(
            loader(&ESP, "\\EFI\\caf\u{e9}.efi", &mut room),
            Err(EncodeError::NotAscii)
        );
    }
}
