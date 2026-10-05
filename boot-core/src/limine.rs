//! The Limine configuration a SlopOS disk boots from, written once at install
//! and never by a commit. It names no `default_entry`: Limine 12.9 consults
//! `LoaderEntryDefault` only while that is unset, so the Boot Loader Interface
//! variable is what chooses the slot.
//!
//! Each slot's kernel and base are named by the boot partition's GUID, which
//! resolves wherever Limine was started from. Another system's firmware entry
//! is offered as an `efi_boot_entry`, which sets `BootNext` to it and resets.

use core::fmt::{self, Write};

use crate::guid::Guid;
use crate::layout::{
    BASE_FILE, ENTRY_PREFIX, FALLBACK_LOADER, FIRMWARE_ENTRY, KERNEL_FILE, LOADER, SLOTS_DIR,
    valid_slot,
};
use crate::load_option::LoadOption;

/// Limine copies a boot entry name into 127 UTF-16 units.
const FIRMWARE_TITLE_MAX: usize = 127;

#[derive(Clone, Copy)]
pub enum MenuEntry<'a> {
    /// The entry `slopos-<slot>`, booting the slot's kernel and base with
    /// `cmdline` appended to the shared one.
    Slot { slot: &'a str, cmdline: &'a str },
    /// Another system, booted through its firmware entry of this description.
    Firmware { title: &'a str },
}

pub struct Config<'a> {
    pub timeout: u32,
    pub serial: bool,
    pub boot_partition: Guid,
    /// The root partition the disk carries; every slot is booted with it as
    /// `root=` in place of any the shared command line names.
    pub root_partition: Option<Guid>,
    pub cmdline: &'a str,
    pub resolution: Option<(u32, u32)>,
    pub entries: &'a [MenuEntry<'a>],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// A slot name the boot partition's writer cannot create.
    BadSlot,
    /// A title Limine would read as menu structure or a macro, or could not
    /// match against a firmware entry.
    BadTitle,
    /// Two entries Limine would tell apart only by a suffix of its own.
    Duplicate,
    /// A command line holding a line break or a macro.
    BadCmdline,
    Format,
}

impl From<fmt::Error> for RenderError {
    fn from(_: fmt::Error) -> Self {
        RenderError::Format
    }
}

/// Whether a firmware entry's description can stand as a menu title and an
/// `entry:` value Limine matches: printable ASCII, short enough to be compared
/// whole, without a macro (`$`), without what Limine escapes in an entry path
/// (`/`, `\`, `#`), and not opening with what it reads as menu structure.
pub fn firmware_title_ok(title: &str) -> bool {
    !title.is_empty()
        && title.len() <= FIRMWARE_TITLE_MAX
        && title
            .bytes()
            .all(|b| (0x20..0x7F).contains(&b) && !matches!(b, b'$' | b'/' | b'\\' | b'#'))
        && !title.starts_with(['+', ' '])
        && !title.ends_with(' ')
}

fn cmdline_ok(cmdline: &str) -> bool {
    !cmdline.contains(['\n', '\r', '$'])
}

/// An entry's menu name: `slopos-<slot>`, or another system's title.
fn name_of<'a>(entry: &MenuEntry<'a>) -> impl Iterator<Item = u8> + Clone + 'a {
    let (prefix, rest) = match *entry {
        MenuEntry::Slot { slot, .. } => (ENTRY_PREFIX, slot),
        MenuEntry::Firmware { title } => ("", title),
    };
    prefix.bytes().chain(rest.bytes())
}

/// The Boot Loader Interface identifier Limine gives a top-level entry of
/// this name: bytes outside `[A-Za-z0-9+_.@-]` become `-`, and a name of one
/// or two dots gains one. Of two entries of one identifier, Limine renames the
/// later, which would move what `LoaderEntryDefault` names.
fn entry_id(name: impl Iterator<Item = u8> + Clone) -> impl Iterator<Item = u8> {
    let mut len = 0;
    let dots = name.clone().all(|b| {
        len += 1;
        b == b'.'
    }) && (1..=2).contains(&len);
    let id_byte = |b: u8| {
        if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'.' | b'@' | b'-') {
            b
        } else {
            b'-'
        }
    };
    name.map(id_byte).chain(dots.then_some(b'-'))
}

/// Whether two entries collide, which `Config::render` refuses: in their
/// identifiers, or, for two firmware entries, in the description
/// `efi_boot_entry` matches without ASCII case. An installer leaves out an
/// offered title that collides with an entry already in its menu.
pub fn collide(a: &MenuEntry<'_>, b: &MenuEntry<'_>) -> bool {
    let titles = match (*a, *b) {
        (MenuEntry::Firmware { title: x }, MenuEntry::Firmware { title: y }) => {
            x.eq_ignore_ascii_case(y)
        }
        _ => false,
    };
    titles || entry_id(name_of(a)).eq(entry_id(name_of(b)))
}

impl Config<'_> {
    pub fn render(&self, out: &mut impl Write) -> Result<(), RenderError> {
        if !cmdline_ok(self.cmdline) {
            return Err(RenderError::BadCmdline);
        }
        for (i, entry) in self.entries.iter().enumerate() {
            if self.entries[..i].iter().any(|e| collide(e, entry)) {
                return Err(RenderError::Duplicate);
            }
        }
        writeln!(out, "timeout: {}", self.timeout)?;
        writeln!(out, "serial: {}", if self.serial { "yes" } else { "no" })?;
        writeln!(out, "verbose: yes")?;
        // The menu resets the firmware's pointers and reads them again only on
        // pointer input or after the editor, which every edited laptop boot took.
        writeln!(out, "mouse: no")?;
        for entry in self.entries {
            match *entry {
                MenuEntry::Slot { slot, cmdline } => self.slot_entry(out, slot, cmdline)?,
                MenuEntry::Firmware { title } => {
                    if !firmware_title_ok(title) {
                        return Err(RenderError::BadTitle);
                    }
                    writeln!(out, "/{title}")?;
                    writeln!(out, "    protocol: efi_boot_entry")?;
                    writeln!(out, "    entry: {title}")?;
                }
            }
        }
        Ok(())
    }

    fn slot_entry(&self, out: &mut impl Write, slot: &str, extra: &str) -> Result<(), RenderError> {
        if !valid_slot(slot) {
            return Err(RenderError::BadSlot);
        }
        if !cmdline_ok(extra) {
            return Err(RenderError::BadCmdline);
        }
        let boot = self.boot_partition;
        writeln!(out, "/{ENTRY_PREFIX}{slot}")?;
        writeln!(out, "    protocol: limine")?;
        writeln!(
            out,
            "    path: guid({boot}):{SLOTS_DIR}/{slot}/{KERNEL_FILE}"
        )?;
        write!(out, "    cmdline:")?;
        let mut words = self.cmdline.split_ascii_whitespace();
        match self.root_partition {
            Some(root) => {
                write!(out, " root=PARTUUID={root}")?;
                for word in words.filter(|w| !w.starts_with("root=")) {
                    write!(out, " {word}")?;
                }
            }
            None => words.try_for_each(|word| write!(out, " {word}"))?,
        }
        for word in extra.split_ascii_whitespace() {
            write!(out, " {word}")?;
        }
        writeln!(out)?;
        writeln!(
            out,
            "    module_path: guid({boot}):{SLOTS_DIR}/{slot}/{BASE_FILE}"
        )?;
        writeln!(out, "    module_string: initramfs")?;
        if let Some((width, height)) = self.resolution {
            writeln!(out, "    resolution: {width}x{height}")?;
        }
        Ok(())
    }
}

/// The most `BootOrder` entries Limine's `efi_boot_entry` reads; with more,
/// it refuses every such entry.
const BOOT_ORDER_READ_MAX: usize = 128;

/// Whether Limine's `efi_boot_entry` takes the option variable `raw` for
/// `title`: its description, read from byte 6 whatever the rest holds, is the
/// title up to the case of `a`-`z`, and ends there.
fn limine_takes(raw: &[u8], title: &str) -> bool {
    let fold = |unit: u16| match unit {
        0x61..=0x7A => unit - 0x20,
        _ => unit,
    };
    let Some(text) = raw.get(6..).filter(|_| raw.len() >= 8) else {
        return false;
    };
    let mut units = text
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    title
        .bytes()
        .all(|b| units.next().is_some_and(|u| fold(u) == fold(u16::from(b))))
        && units.next() == Some(0)
}

/// One number `BootOrder` lists, as Limine walks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed<'a> {
    /// No variable of that number exists.
    Absent,
    /// One exists that could not be read, so its description is unknown.
    Unreadable,
    Value(&'a [u8]),
}

fn starts_ours(listed: &Listed<'_>) -> bool {
    let Listed::Value(raw) = *listed else {
        return false;
    };
    LoadOption::parse(raw)
        .ok()
        .and_then(|option| option.installed_loader())
        .is_some_and(|(_, file)| file.is(LOADER))
}

/// The menu title for the option `BootOrder` lists at `index`, `listed` being
/// every number it lists, in order: another system's active loader on a disk
/// partition, described so Limine can match it, and the first of `listed`
/// Limine takes for that description, since it boots the first. SlopOS's own
/// entry, registered after the menu is written, already counts: no title of
/// its description is offered, nor any if it would take `BootOrder` past what
/// Limine reads. `own_fallback` is the ESP SlopOS created, whose
/// removable-media path is its own.
pub fn offered_title<'t>(
    listed: &[Listed<'_>],
    index: usize,
    own_fallback: Option<&Guid>,
    out: &'t mut [u8],
) -> Option<&'t str> {
    let registered = listed.len() + usize::from(!listed.iter().any(starts_ours));
    let Listed::Value(raw) = *listed.get(index)? else {
        return None;
    };
    let option = LoadOption::parse(raw).ok()?;
    let (partition, file) = option.installed_loader()?;
    let ours =
        file.is(LOADER) || (own_fallback == Some(&partition.partition) && file.is(FALLBACK_LOADER));
    if registered > BOOT_ORDER_READ_MAX || !option.is_active() || ours {
        return None;
    }
    let mut len = 0;
    for unit in option.description_units() {
        let byte = u8::try_from(unit).ok().filter(u8::is_ascii)?;
        *out.get_mut(len)? = byte;
        len += 1;
    }
    let title = core::str::from_utf8(&out[..len]).ok()?;
    let first = listed.iter().position(|entry| match *entry {
        Listed::Absent => false,
        Listed::Unreadable => true,
        Listed::Value(raw) => limine_takes(raw, title),
    });
    let posing = title.eq_ignore_ascii_case(FIRMWARE_ENTRY);
    (firmware_title_ok(title) && first == Some(index) && !posing).then_some(title)
}

#[cfg(test)]
mod tests;
