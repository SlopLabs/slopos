//! The Boot Loader Interface: the variables, under
//! [`crate::variables::LOADER`], through which a loader tells the booted system
//! what it booted and the system tells the loader what to boot next. Every
//! value is a UTF-16LE string with a terminator; `LoaderEntries` is several.

use core::char::DecodeUtf16Error;

/// Set by the system, read on every boot: the entry to boot by default.
pub const ENTRY_DEFAULT: &str = "LoaderEntryDefault";
/// Set by the system, consumed by the loader on the next boot.
pub const ENTRY_ONE_SHOT: &str = "LoaderEntryOneShot";
/// Set by the loader: the entry it booted.
pub const ENTRY_SELECTED: &str = "LoaderEntrySelected";
/// Set by the loader: every entry it offers, in menu order.
pub const ENTRIES: &str = "LoaderEntries";
/// Set by the loader: the partition GUID of the ESP it was started from.
pub const DEVICE_PART_UUID: &str = "LoaderDevicePartUUID";

/// `value` as the interface stores it.
pub fn encode(value: &str) -> impl Iterator<Item = u8> + '_ {
    value
        .encode_utf16()
        .chain(core::iter::once(0))
        .flat_map(u16::to_le_bytes)
}

/// One string of a value, without its terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utf16<'a>(&'a [u8]);

impl<'a> Utf16<'a> {
    pub fn chars(&self) -> impl Iterator<Item = Result<char, DecodeUtf16Error>> + 'a {
        char::decode_utf16(
            self.0
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
        )
    }
}

/// The strings a value holds, split at terminators. A run after the last
/// terminator counts too: a loader that wrote one without it still named it.
pub fn strings(raw: &[u8]) -> impl Iterator<Item = Utf16<'_>> {
    let units = &raw[..raw.len() - raw.len() % 2];
    let mut rest = units;
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .chunks_exact(2)
            .position(|pair| pair == [0, 0])
            .unwrap_or(rest.len() / 2);
        let (string, tail) = rest.split_at(2 * end);
        rest = tail.get(2..).unwrap_or(&[]);
        Some(Utf16(string))
    })
}

/// A `LoaderEntryDefault` that names no entry the loader offers. Limine also
/// accepts a menu path there, so which entry it boots cannot be told from the
/// identifiers alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownDefault;

/// What a Limine configuration without `default_entry` boots when no one-shot
/// is armed: the entry `LoaderEntryDefault` names, or the first when it is
/// unset.
pub fn default_entry<'a>(
    entries: &[&'a str],
    default: Option<&str>,
) -> Result<Option<&'a str>, UnknownDefault> {
    match default {
        None => Ok(entries.first().copied()),
        Some(wanted) => entries
            .iter()
            .find(|&&e| e == wanted)
            .map(|&e| Some(e))
            .ok_or(UnknownDefault),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn texts(raw: &[u8]) -> Vec<String> {
        strings(raw)
            .map(|s| s.chars().map(|c| c.unwrap()).collect())
            .collect()
    }

    #[test]
    fn a_value_round_trips() {
        let raw: Vec<u8> = encode("slopos-a").collect();
        assert_eq!(&raw[..4], &[b's', 0, b'l', 0]);
        assert_eq!(&raw[raw.len() - 2..], &[0, 0]);
        assert_eq!(texts(&raw), ["slopos-a"]);
    }

    #[test]
    fn loader_entries_split_at_each_terminator() {
        let raw: Vec<u8> = ["slopos-a", "slopos-b", "slopos-bad"]
            .iter()
            .flat_map(|e| encode(e))
            .collect();
        assert_eq!(texts(&raw), ["slopos-a", "slopos-b", "slopos-bad"]);
        assert_eq!(
            texts(&raw[..raw.len() - 2]),
            ["slopos-a", "slopos-b", "slopos-bad"]
        );
        assert!(texts(&[]).is_empty());
    }

    #[test]
    fn the_default_is_the_variable_s_entry_or_the_first() {
        let entries = ["slopos-a", "slopos-b"];
        assert_eq!(default_entry(&entries, None), Ok(Some("slopos-a")));
        assert_eq!(
            default_entry(&entries, Some("slopos-b")),
            Ok(Some("slopos-b"))
        );
        assert_eq!(default_entry(&[], None), Ok(None));
    }

    #[test]
    fn a_default_naming_no_offered_entry_is_unknown() {
        let entries = ["slopos-a", "slopos-b"];
        for value in ["cachyos.conf", "slopos-b#0", "slopos-"] {
            assert_eq!(default_entry(&entries, Some(value)), Err(UnknownDefault));
        }
    }
}
