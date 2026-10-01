//! UEFI variable namespaces SlopOS touches, and what a write to the firmware's
//! boot manager variables must look like before it reaches the firmware.
//!
//! Firmware stores what it is given and parses it again on every boot, often
//! before any recovery is possible, so a malformed `Boot####` or `BootOrder`
//! is refused here rather than discovered by the machine.

use crate::device_path;
use crate::guid::Guid;
use crate::load_option::LoadOption;

/// `EFI_GLOBAL_VARIABLE`: the boot manager's own variables.
pub const GLOBAL: Guid = Guid::from_spelling("8be4df61-93ca-11d2-aa0d-00e098032b8c");
/// The Boot Loader Interface's vendor GUID, under which a loader and the
/// booted system agree on what boots next.
pub const LOADER: Guid = Guid::from_spelling("4a67b082-0a4c-41cf-b6c7-440b29bb8c4f");
/// SlopOS's own.
pub const SLOPOS: Guid = Guid::from_spelling("5a1b0b05-5105-4e57-a11e-0000000000a1");

pub const NON_VOLATILE: u32 = 0x1;
pub const BOOTSERVICE_ACCESS: u32 = 0x2;
pub const RUNTIME_ACCESS: u32 = 0x4;
/// Kept across resets, readable by the boot manager and the running system:
/// what UEFI 2.10 §3.3 gives every boot manager variable a system may write,
/// and what a loader's next-boot variables need to survive the reset to it.
pub const PERSISTENT: u32 = NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS;

/// The global variables an installer needs: the load options, their order,
/// the one-boot override, and which option the firmware booted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootVariable {
    Option(u16),
    Order,
    Next,
    Current,
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

impl BootVariable {
    /// `Boot####` takes uppercase hex digits only (§3.1.1): a firmware matches
    /// names exactly, so `Boot000a` would be a variable nothing boots.
    pub fn from_name(name: &str) -> Option<BootVariable> {
        match name {
            "BootOrder" => return Some(BootVariable::Order),
            "BootNext" => return Some(BootVariable::Next),
            "BootCurrent" => return Some(BootVariable::Current),
            _ => {}
        }
        let digits = name.strip_prefix("Boot")?.as_bytes();
        if digits.len() != 4 || !digits.iter().all(|d| HEX_UPPER.contains(d)) {
            return None;
        }
        let text = core::str::from_utf8(digits).ok()?;
        u16::from_str_radix(text, 16).ok().map(BootVariable::Option)
    }

    pub fn option_name(number: u16) -> [u8; 8] {
        let mut name = *b"Boot0000";
        for (i, slot) in name[4..].iter_mut().enumerate() {
            *slot = HEX_UPPER[usize::from(number >> (12 - 4 * i) & 0xF)];
        }
        name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Written by the firmware alone.
    ReadOnly,
    /// Not [`PERSISTENT`]: no authenticated, append or hardware
    /// error record write reaches the boot manager's namespace.
    Attributes,
    Malformed,
}

/// Hold a write of `data` to `variable` to what its consumer parses. An empty
/// `data` deletes, which a load option and `BootNext` allow and `BootOrder`
/// does not: without it a firmware boots whatever it finds first.
pub fn check_boot_write(
    variable: BootVariable,
    attributes: u32,
    data: &[u8],
) -> Result<(), Refusal> {
    if variable == BootVariable::Current {
        return Err(Refusal::ReadOnly);
    }
    if attributes != PERSISTENT {
        return Err(Refusal::Attributes);
    }
    let well_formed = match variable {
        BootVariable::Option(_) => {
            data.is_empty()
                || LoadOption::parse(data)
                    .is_ok_and(|option| device_path::validate(option.file_path_list).is_ok())
        }
        BootVariable::Order => !data.is_empty() && data.len() % 2 == 0,
        BootVariable::Next => data.is_empty() || data.len() == 2,
        BootVariable::Current => false,
    };
    well_formed.then_some(()).ok_or(Refusal::Malformed)
}

pub fn boot_order(raw: &[u8]) -> impl Iterator<Item = u16> + '_ {
    raw.chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
}

/// `order` with `number` placed once: first when `first`, otherwise where it
/// first stood, or at the end if it was not listed. Every other number keeps
/// its place.
pub fn place_in_order(order: &[u16], number: u16, first: bool) -> impl Iterator<Item = u16> + '_ {
    let kept = (!first)
        .then(|| order.iter().position(|&n| n == number))
        .flatten();
    let appended = !first && kept.is_none();
    first
        .then_some(number)
        .into_iter()
        .chain(
            order
                .iter()
                .enumerate()
                .filter(move |&(i, &n)| n != number || Some(i) == kept)
                .map(|(_, &n)| n),
        )
        .chain(appended.then_some(number))
}

/// What a `Boot####` variable holds, as registering a loader sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    Absent,
    /// An entry starting the loader being registered.
    Ours,
    /// Anything else, an entry too large or too damaged to read included.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Existing(u16),
    New(u16),
}

/// Where a loader's entry goes: an entry already starting it, one `BootOrder`
/// lists if there is one, or a new entry at the lowest number that is neither
/// a variable nor listed in `BootOrder`, where a dangling number would put a
/// new entry in another's place. Only numbers up to that one and those
/// `BootOrder` lists are looked at, since firmware offers no enumeration short
/// of probing all 65536. `None` when every number is in use.
pub fn place_entry<E>(
    order: &[u16],
    mut held: impl FnMut(u16) -> Result<Held, E>,
) -> Result<Option<Placement>, E> {
    let mut found = None;
    let mut found_listed = None;
    let mut note = |number: u16| {
        found.get_or_insert(number);
        if order.contains(&number) {
            found_listed.get_or_insert(number);
        }
    };
    let mut free = None;
    for number in 0..=u16::MAX {
        match held(number)? {
            Held::Ours => note(number),
            Held::Other => {}
            Held::Absent if order.contains(&number) => {}
            Held::Absent => {
                free = Some(number);
                break;
            }
        }
    }
    if let Some(free) = free {
        for &number in order.iter().filter(|&&n| n > free) {
            if held(number)? == Held::Ours {
                note(number);
            }
        }
    }
    Ok(found_listed
        .or(found)
        .map(Placement::Existing)
        .or(free.map(Placement::New)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_path::HardDrive;
    use crate::load_option;
    use std::vec;

    #[test]
    fn only_the_boot_manager_names_classify() {
        assert_eq!(
            BootVariable::from_name("Boot0000"),
            Some(BootVariable::Option(0))
        );
        assert_eq!(
            BootVariable::from_name("Boot0A1F"),
            Some(BootVariable::Option(0x0A1F))
        );
        assert_eq!(
            BootVariable::from_name("BootOrder"),
            Some(BootVariable::Order)
        );
        assert_eq!(
            BootVariable::from_name("BootNext"),
            Some(BootVariable::Next)
        );
        assert_eq!(
            BootVariable::from_name("BootCurrent"),
            Some(BootVariable::Current)
        );
        for refused in [
            "Boot0a1f",
            "Boot000",
            "Boot00000",
            "Boot+001",
            "Driver0000",
            "PK",
            "Boot",
        ] {
            assert_eq!(BootVariable::from_name(refused), None, "{refused}");
        }
    }

    #[test]
    fn an_option_name_is_four_uppercase_digits() {
        assert_eq!(&BootVariable::option_name(0x0A1F), b"Boot0A1F");
        assert_eq!(&BootVariable::option_name(0), b"Boot0000");
        assert_eq!(&BootVariable::option_name(0xFFFF), b"BootFFFF");
    }

    #[test]
    fn writes_are_held_to_their_formats() {
        let esp = HardDrive {
            partition_number: 1,
            start_lba: 2048,
            blocks: 1024,
            partition: Guid::from_spelling("6d3c2a91-5b0e-4b6f-9a51-2f0c3e4d5a6b"),
        };
        let mut path = vec![0u8; device_path::loader_len(r"\EFI\X\A.EFI")];
        device_path::loader(&esp, r"\EFI\X\A.EFI", &mut path).unwrap();
        let mut option = vec![0u8; load_option::encoded_len("X", &path)];
        load_option::encode(load_option::ACTIVE, "X", &path, &mut option).unwrap();

        let attrs = PERSISTENT;
        let opt = BootVariable::Option(3);
        assert_eq!(check_boot_write(opt, attrs, &option), Ok(()));
        assert_eq!(check_boot_write(opt, attrs, &[]), Ok(()));
        assert_eq!(
            check_boot_write(opt, attrs, &option[..9]),
            Err(Refusal::Malformed)
        );
        let mut open_ended = option.clone();
        let end = open_ended.len() - 4;
        open_ended[end] = 0x04;
        assert_eq!(
            check_boot_write(opt, attrs, &open_ended),
            Err(Refusal::Malformed)
        );
        assert_eq!(
            check_boot_write(opt, attrs | 0x20, &option),
            Err(Refusal::Attributes)
        );
        assert_eq!(
            check_boot_write(opt, NON_VOLATILE, &option),
            Err(Refusal::Attributes)
        );

        let order = BootVariable::Order;
        assert_eq!(check_boot_write(order, attrs, &[1, 0, 2, 0]), Ok(()));
        assert_eq!(
            check_boot_write(order, attrs, &[1, 0, 2]),
            Err(Refusal::Malformed)
        );
        assert_eq!(check_boot_write(order, attrs, &[]), Err(Refusal::Malformed));

        let next = BootVariable::Next;
        assert_eq!(check_boot_write(next, attrs, &[1, 0]), Ok(()));
        assert_eq!(check_boot_write(next, attrs, &[]), Ok(()));
        assert_eq!(
            check_boot_write(next, attrs, &[1, 0, 0, 0]),
            Err(Refusal::Malformed)
        );

        assert_eq!(
            check_boot_write(BootVariable::Current, attrs, &[1, 0]),
            Err(Refusal::ReadOnly)
        );
    }

    fn placed(order: &[u16], number: u16, first: bool) -> std::vec::Vec<u16> {
        place_in_order(order, number, first).collect()
    }

    #[test]
    fn an_entry_goes_first_only_when_asked() {
        assert_eq!(placed(&[1, 2, 3], 7, true), [7, 1, 2, 3]);
        assert_eq!(placed(&[1, 2, 3], 2, true), [2, 1, 3]);
        assert_eq!(placed(&[], 7, true), [7]);
    }

    #[test]
    fn otherwise_an_entry_keeps_its_place_or_joins_the_end() {
        assert_eq!(placed(&[1, 2, 3], 7, false), [1, 2, 3, 7]);
        assert_eq!(placed(&[1, 2, 3], 2, false), [1, 2, 3]);
        assert_eq!(placed(&[], 7, false), [7]);
    }

    #[test]
    fn an_entry_is_listed_once() {
        assert_eq!(placed(&[2, 1, 2, 3, 2], 2, false), [2, 1, 3]);
        assert_eq!(placed(&[1, 2, 1], 2, true), [2, 1, 1]);
    }

    /// `place_entry` over a firmware holding `vars`, numbered from zero.
    fn placement(order: &[u16], vars: &[Held]) -> Option<Placement> {
        place_entry::<()>(order, |n| {
            Ok(vars.get(usize::from(n)).copied().unwrap_or(Held::Absent))
        })
        .unwrap()
    }

    #[test]
    fn a_new_entry_takes_the_lowest_number_nothing_holds_or_lists() {
        use Held::*;
        assert_eq!(placement(&[0, 1], &[Other, Other]), Some(Placement::New(2)));
        assert_eq!(
            placement(&[0, 2, 1], &[Other, Other, Absent]),
            Some(Placement::New(3)),
            "a number BootOrder still lists is no free one"
        );
        assert_eq!(placement(&[], &[]), Some(Placement::New(0)));
    }

    #[test]
    fn an_existing_entry_is_reused_preferring_one_boot_order_lists() {
        use Held::*;
        assert_eq!(
            placement(&[0], &[Other, Ours]),
            Some(Placement::Existing(1))
        );
        assert_eq!(
            placement(&[0, 3], &[Other, Ours, Other, Ours]),
            Some(Placement::Existing(3))
        );
        let mut vars = std::vec![Other; 9];
        vars[4] = Absent;
        vars[8] = Ours;
        assert_eq!(
            placement(&[0, 8], &vars),
            Some(Placement::Existing(8)),
            "an entry past the first gap is found through BootOrder"
        );
    }

    #[test]
    fn an_unprobeable_variable_stops_the_search() {
        let mut probes = 0;
        let got = place_entry(&[], |n| {
            probes += 1;
            if n == 2 {
                Err("ENODEV")
            } else {
                Ok(Held::Other)
            }
        });
        assert_eq!(got, Err("ENODEV"));
        assert_eq!(probes, 3);
    }

    #[test]
    fn a_boot_order_lists_little_endian_numbers() {
        assert!(boot_order(&[1, 0, 0x1F, 0x0A]).eq([1, 0x0A1F]));
    }
}
