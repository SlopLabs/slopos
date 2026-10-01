//! `EFI_LOAD_OPTION`, UEFI Specification 2.10 §3.1.3: what a `Boot####`
//! variable holds. Attributes, the length of the device path list, a
//! NUL-terminated UCS-2 description, the device paths, then optional data
//! handed to the loaded image.

use crate::device_path::{self, FilePath, HardDrive};
use crate::guid::Guid;

pub const ACTIVE: u32 = 0x0000_0001;

const FIXED: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    Short,
    /// No terminator before the device path list would have to start.
    UnterminatedDescription,
    /// `FilePathListLength` reaches past the option.
    FilePathListLength,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadOption<'a> {
    pub attributes: u32,
    /// UCS-2, little-endian, without the terminator.
    pub description: &'a [u8],
    pub file_path_list: &'a [u8],
    pub optional_data: &'a [u8],
}

impl<'a> LoadOption<'a> {
    /// Decode `raw`'s framing. The device path list is bounded but not walked:
    /// [`device_path::validate`] holds it to being well formed where that
    /// matters, before anything is written.
    pub fn parse(raw: &'a [u8]) -> Result<Self, Malformed> {
        if raw.len() < FIXED {
            return Err(Malformed::Short);
        }
        let attributes = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let list_len = usize::from(u16::from_le_bytes([raw[4], raw[5]]));
        let text = &raw[FIXED..];
        let units = text.len() / 2;
        let end = (0..units)
            .find(|&i| text[2 * i] == 0 && text[2 * i + 1] == 0)
            .ok_or(Malformed::UnterminatedDescription)?;
        let list_at = FIXED + 2 * (end + 1);
        let file_path_list = raw
            .get(list_at..list_at + list_len)
            .ok_or(Malformed::FilePathListLength)?;
        Ok(LoadOption {
            attributes,
            description: &text[..2 * end],
            file_path_list,
            optional_data: &raw[list_at + list_len..],
        })
    }

    pub fn is_active(&self) -> bool {
        self.attributes & ACTIVE != 0
    }

    pub fn description_units(&self) -> impl Iterator<Item = u16> + 'a {
        self.description
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
    }

    /// The partition and file of a loader installed on a disk; `None` for any
    /// other kind of entry.
    pub fn installed_loader(&self) -> Option<(HardDrive, FilePath<'a>)> {
        device_path::hard_drive_file(self.file_path_list)
    }

    /// Whether this option starts the loader at `path` on the partition
    /// `partition`: the identity systemd's `bootctl` matches an entry by.
    pub fn starts(&self, partition: &Guid, path: &str) -> bool {
        self.installed_loader()
            .is_some_and(|(hd, file)| hd.partition == *partition && file.is(path))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    TooSmall,
    /// A NUL would end the description early.
    NulInDescription,
}

pub fn encoded_len(description: &str, file_path_list: &[u8]) -> usize {
    FIXED + 2 * (description.encode_utf16().count() + 1) + file_path_list.len()
}

/// An option with no optional data, as an installer registers a loader.
pub fn encode(
    attributes: u32,
    description: &str,
    file_path_list: &[u8],
    out: &mut [u8],
) -> Result<usize, EncodeError> {
    if description.contains('\0') {
        return Err(EncodeError::NulInDescription);
    }
    let len = encoded_len(description, file_path_list);
    let list_len = u16::try_from(file_path_list.len()).map_err(|_| EncodeError::TooSmall)?;
    let out = out.get_mut(..len).ok_or(EncodeError::TooSmall)?;
    out[..4].copy_from_slice(&attributes.to_le_bytes());
    out[4..6].copy_from_slice(&list_len.to_le_bytes());
    let mut at = FIXED;
    for unit in description.encode_utf16().chain(core::iter::once(0)) {
        out[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        at += 2;
    }
    out[at..].copy_from_slice(file_path_list);
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const ESP: HardDrive = HardDrive {
        partition_number: 1,
        start_lba: 2048,
        blocks: 532_480,
        partition: Guid::from_spelling("6d3c2a91-5b0e-4b6f-9a51-2f0c3e4d5a6b"),
    };
    const LOADER: &str = r"\EFI\SlopOS\BOOTX64.EFI";

    fn option(description: &str) -> Vec<u8> {
        let mut path = vec![0u8; device_path::loader_len(LOADER)];
        device_path::loader(&ESP, LOADER, &mut path).unwrap();
        let mut out = vec![0u8; encoded_len(description, &path)];
        assert_eq!(encode(ACTIVE, description, &path, &mut out), Ok(out.len()));
        out
    }

    #[test]
    fn an_encoded_option_has_the_spec_layout_and_parses_back() {
        let raw = option("SlopOS");
        assert_eq!(&raw[..4], &ACTIVE.to_le_bytes());
        let list_len = device_path::loader_len(LOADER) as u16;
        assert_eq!(&raw[4..6], &list_len.to_le_bytes());
        assert_eq!(
            &raw[6..20],
            &[b'S', 0, b'l', 0, b'o', 0, b'p', 0, b'O', 0, b'S', 0, 0, 0]
        );
        let parsed = LoadOption::parse(&raw).unwrap();
        assert!(parsed.is_active());
        assert!(parsed.description_units().eq("SlopOS".encode_utf16()));
        assert!(parsed.optional_data.is_empty());
        assert!(parsed.starts(&ESP.partition, LOADER));
        assert!(!parsed.starts(&ESP.partition, r"\EFI\BOOT\BOOTX64.EFI"));
    }

    #[test]
    fn optional_data_follows_the_device_path_list() {
        let mut raw = option("SlopOS");
        raw.extend_from_slice(b"\x01\x02");
        assert_eq!(LoadOption::parse(&raw).unwrap().optional_data, b"\x01\x02");
    }

    #[test]
    fn a_damaged_option_is_refused() {
        let raw = option("SlopOS");
        assert_eq!(LoadOption::parse(&raw[..5]), Err(Malformed::Short));
        let mut long_list = raw.clone();
        long_list[4] = 0xFF;
        assert_eq!(
            LoadOption::parse(&long_list),
            Err(Malformed::FilePathListLength)
        );
        let unterminated: Vec<u8> = raw[..6].iter().copied().chain([b'S', 0]).collect();
        assert_eq!(
            LoadOption::parse(&unterminated),
            Err(Malformed::UnterminatedDescription)
        );
        let mut cut_end = raw.clone();
        let end = cut_end.len() - 4;
        cut_end[end] = 0x04;
        let parsed = LoadOption::parse(&cut_end).unwrap();
        assert!(device_path::validate(parsed.file_path_list).is_err());
    }

    #[test]
    fn another_partition_is_another_loader() {
        let raw = option("SlopOS");
        let other = Guid::from_spelling("00000000-0000-0000-0000-000000000001");
        assert!(!LoadOption::parse(&raw).unwrap().starts(&other, LOADER));
    }

    #[test]
    fn a_description_holding_a_nul_is_refused() {
        let mut out = [0u8; 256];
        assert_eq!(
            encode(ACTIVE, "Slop\0OS", &[0x7F, 0xFF, 4, 0], &mut out),
            Err(EncodeError::NulInDescription)
        );
    }
}
