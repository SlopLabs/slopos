//! What the installer's engine and whatever drives it agree on: the machine
//! as the engine read it ([`System`]), what to install ([`Choices`]), what an
//! install would do ([`Proposal`]) or why it cannot ([`Issue`]), what an
//! install reports ([`Event`]) and how it ends ([`Summary`], [`Failure`]),
//! the frames that carry them between processes ([`protocol`]), and the
//! window's pages over them ([`wizard`]).
//!
//! Free of I/O and `unsafe`: the engine reads and writes the disks, and the
//! host tests everything both sides read.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod choices;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod issue;
mod layout;
mod proposal;
pub mod protocol;
mod system;
pub mod wizard;

pub use choices::{Choices, DEFAULT_CMDLINE, DEFAULT_REMOTE, FIRST_PERSON, Mode, Person};
pub use issue::{Event, Failure, Issue, STEPS, Summary, role_words};
pub use layout::{Layout, Piece, Segment};
pub use proposal::{Action, FirstPerson, Part, Planned, Proposal, Setup, Span, Table, Tag, Target};
pub use system::{
    Contents, Disk, DiskState, Fit, Listing, MbrPartition, Medium, Opened, Refusal, Reinstall,
    System,
};

use alloc::format;
use alloc::string::String;

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::{self, Partition};
use slopos_boot_core::layout::{BOOT_TYPE, CRASH_TYPE, ESP_TYPE, ROOT_TYPE};

/// What a root holds without the payload: the system's own writes.
const ROOT_MIN_BYTES: u64 = 1 << 30;
/// The room a root keeps beside the payload for building the system, which a
/// clean dev and tests build holds 3.8 GiB of.
const ROOT_WORK_BYTES: u64 = 6 << 30;

/// A root's least size: the payload of `payload` bytes and room to build
/// beside it, or the system's own needs without one.
pub fn root_min(payload: Option<u64>) -> u64 {
    payload.map_or(ROOT_MIN_BYTES, |bytes| bytes + ROOT_WORK_BYTES)
}

pub fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A memory or file size, in binary units.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A disk's size in decimal units, as its label gives it: `1 TB`, `34.4 GB`,
/// `512 MB`.
pub fn disk_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 999.5 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    let said = match (unit, value >= 100.0) {
        (0, _) if bytes == 1 => return "1 byte".into(),
        (0, _) => format!("{bytes}"),
        (_, true) => format!("{value:.0}"),
        (_, false) => format!("{value:.1}"),
    };
    let said = said.strip_suffix(".0").unwrap_or(&said);
    format!("{said} {}", UNITS[unit])
}

/// What a disk's maker calls it, from the names `/dev/disk/by-id` gives it
/// as udev does: an NVMe disk's model, a USB disk's vendor and product. The
/// serial each name carries last is dropped, as is the namespace or LUN
/// after it, and the `_` udev puts for a space is a space again. A USB disk
/// without the serial Bulk-Only requires loses its product's last word.
pub fn model_of<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<String> {
    fn digits(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
    }
    let names: alloc::vec::Vec<&str> = names.into_iter().collect();
    let identified = names.iter().find_map(|&name| {
        if let Some(rest) = name.strip_prefix("nvme-") {
            let alias = names.iter().any(|other| {
                other
                    .strip_prefix(name)
                    .and_then(|tail| tail.strip_prefix('_'))
                    .is_some_and(digits)
            });
            match alias {
                true => Some(rest),
                false => rest
                    .rsplit_once('_')
                    .filter(|(_, n)| digits(n))
                    .map(|(m, _)| m),
            }
        } else {
            let rest = name.strip_prefix("usb-")?;
            let (rest, lun) = rest.rsplit_once("-0:")?;
            digits(lun).then_some(rest)
        }
    })?;
    let (model, _serial) = identified.rsplit_once('_')?;
    let words: alloc::vec::Vec<&str> = model.split('_').filter(|w| !w.is_empty()).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A size as the CLI takes one: a whole number of bytes, a number of `kB` to
/// `TB` as the listing gives sizes, or a whole number of `K` to `T`, each 1024
/// of the one before, with or without `iB`.
pub fn parse_size(text: &str) -> Option<u64> {
    if ["kB", "MB", "GB", "TB"]
        .iter()
        .any(|unit| text.ends_with(unit))
    {
        return parse_disk_size(text);
    }
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let scale: u64 = match unit {
        "" | "B" => 1,
        "K" | "KiB" => 1 << 10,
        "M" | "MiB" => 1 << 20,
        "G" | "GiB" => 1 << 30,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(scale)
}

/// A size as the window's box takes one, in the decimal units it shows
/// sizes in: `20`, `20 GB`, `512mb`, `1.5 TB`, or with `KiB` to `TiB`; a
/// bare number is GB.
pub fn parse_disk_size(text: &str) -> Option<u64> {
    let text: String = text.split_whitespace().collect();
    let digits = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let scale: u128 = match unit.to_ascii_lowercase().as_str() {
        "" | "g" | "gb" => 1_000_000_000,
        "b" | "bytes" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let tenths = 10u128.pow(fraction.len() as u32);
    let fraction: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    let bytes = whole
        .checked_mul(scale)?
        .checked_add(fraction * scale / tenths)?;
    u64::try_from(bytes).ok()
}

/// What a partition type GUID names, as `fdisk` would say it.
pub fn type_name(guid: &Guid) -> String {
    const KNOWN: [(&str, &str); 10] = [
        ("0fc63daf-8483-4772-8e79-3d69d8477de4", "Linux filesystem"),
        (
            "4f68bce3-e8cd-4db1-96e7-fbcaf984b709",
            "Linux root (x86-64)",
        ),
        ("933ac7e1-2eb4-4f13-b844-0e14e2aef915", "Linux home"),
        ("0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", "Linux swap"),
        (
            "bc13c2ff-59e6-4262-a352-b275fd6f7172",
            "Linux extended boot",
        ),
        ("e6d6d379-f507-44c2-a23c-238f2a3df928", "Linux LVM"),
        ("a19d880f-05fc-4d3b-a006-743f0f84911e", "Linux RAID"),
        (
            "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
            "Microsoft basic data",
        ),
        ("e3c9e316-0b5c-4db8-817d-f92df00215ae", "Microsoft reserved"),
        ("de94bba4-06d1-4d40-a16a-bfd50179d6ac", "Windows recovery"),
    ];
    match *guid {
        ESP_TYPE => "EFI system".into(),
        BOOT_TYPE => "SlopOS boot".into(),
        ROOT_TYPE => "SlopOS root".into(),
        CRASH_TYPE => "SlopOS crash".into(),
        _ => KNOWN
            .iter()
            .find(|(spelling, _)| Guid::parse(spelling) == Some(*guid))
            .map_or_else(|| format!("{guid}"), |(_, name)| (*name).into()),
    }
}

/// A partition's name in its table entry.
pub fn name_of(p: &Partition) -> String {
    char::decode_utf16(p.entry.name.iter().copied().take_while(|&u| u != 0))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// As the kernel names it: `/dev/nvme0n1p3`, `/dev/vda3`.
pub fn partition_node(disk: &str, number: u32) -> String {
    format!("{disk}{}{number}", gpt::partition_separator(disk))
}

/// A node without its directory: `nvme0n1` for `/dev/nvme0n1`.
pub fn short(node: &str) -> &str {
    node.rsplit('/').next().unwrap_or(node)
}

#[cfg(test)]
mod tests;
