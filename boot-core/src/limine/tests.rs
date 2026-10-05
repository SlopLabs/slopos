use super::*;
use crate::device_path::{self, HardDrive};
use crate::load_option::{self, ACTIVE};
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

const BOOT: Guid = Guid::from_spelling("11111111-2222-4333-8444-555555555555");
const ROOT: Guid = Guid::from_spelling("66666666-7777-4888-9999-aaaaaaaaaaaa");

const SLOTS: [MenuEntry<'static>; 2] = [
    MenuEntry::Slot {
        slot: "a",
        cmdline: "slot=a",
    },
    MenuEntry::Slot {
        slot: "b",
        cmdline: "slot=b",
    },
];

fn config<'a>(entries: &'a [MenuEntry<'a>], cmdline: &'a str) -> Config<'a> {
    Config {
        timeout: 0,
        serial: false,
        boot_partition: BOOT,
        root_partition: None,
        cmdline,
        resolution: None,
        entries,
    }
}

fn rendered(config: &Config<'_>) -> String {
    let mut out = String::new();
    config.render(&mut out).unwrap();
    out
}

#[test]
fn slots_boot_from_the_boot_partition_and_nothing_names_a_default() {
    let text = rendered(&Config {
        serial: true,
        resolution: Some((1920, 1080)),
        ..config(&SLOTS, "tests=on root=auto")
    });
    let boot = "guid(11111111-2222-4333-8444-555555555555)";
    let expected = format!(
        "timeout: 0\nserial: yes\nverbose: yes\n\
         /slopos-a\n    protocol: limine\n    path: {boot}:/boot/a/kernel.elf\n    \
         cmdline: tests=on root=auto slot=a\n    module_path: {boot}:/boot/a/base.img\n    \
         module_string: initramfs\n    resolution: 1920x1080\n\
         /slopos-b\n    protocol: limine\n    path: {boot}:/boot/b/kernel.elf\n    \
         cmdline: tests=on root=auto slot=b\n    module_path: {boot}:/boot/b/base.img\n    \
         module_string: initramfs\n    resolution: 1920x1080\n"
    );
    assert_eq!(text, expected);
    assert!(!text.contains("default_entry"));
}

#[test]
fn a_root_partition_replaces_every_root_the_command_line_names() {
    let text = rendered(&Config {
        timeout: 3,
        root_partition: Some(ROOT),
        ..config(&SLOTS[..1], "root=auto quiet root=initramfs")
    });
    assert!(text.contains(
        "    cmdline: root=PARTUUID=66666666-7777-4888-9999-aaaaaaaaaaaa quiet slot=a\n"
    ));
    assert!(text.starts_with("timeout: 3\nserial: no\n"));
    assert!(!text.contains("resolution"));
}

#[test]
fn another_system_is_offered_through_its_firmware_entry() {
    let entries = [SLOTS[0], MenuEntry::Firmware { title: "cachyos" }];
    let text = rendered(&Config {
        timeout: 5,
        ..config(&entries, "")
    });
    assert!(text.ends_with("/cachyos\n    protocol: efi_boot_entry\n    entry: cachyos\n"));
}

#[test]
fn titles_and_command_lines_that_would_break_the_file_are_refused() {
    let mut sink = String::new();
    for title in [
        "/sub",
        "a/b",
        "a\\b",
        "+dir",
        "",
        "caf\u{e9}",
        " padded",
        "padded ",
        "${ARCH}",
        "a#b",
    ] {
        let entries = [MenuEntry::Firmware { title }];
        assert_eq!(
            config(&entries, "").render(&mut sink),
            Err(RenderError::BadTitle),
            "{title:?}"
        );
    }
    let long = "x".repeat(128);
    let entries = [MenuEntry::Firmware { title: &long }];
    assert_eq!(
        config(&entries, "").render(&mut sink),
        Err(RenderError::BadTitle)
    );
    let entries = [MenuEntry::Slot {
        slot: "B",
        cmdline: "",
    }];
    assert_eq!(
        config(&entries, "").render(&mut sink),
        Err(RenderError::BadSlot)
    );
    for cmdline in ["a\npath: x", "root=${ROOT}"] {
        assert_eq!(
            config(&SLOTS, cmdline).render(&mut sink),
            Err(RenderError::BadCmdline)
        );
    }
}

/// Limine renames the later of two entries of one identifier, which would
/// move what `LoaderEntryDefault` names.
#[test]
fn entries_of_one_identifier_are_refused() {
    let mut sink = String::new();
    let twice = [SLOTS[0], SLOTS[0]];
    let titles = [
        MenuEntry::Firmware { title: "CachyOS" },
        MenuEntry::Firmware { title: "cachyos" },
    ];
    let mapped = [
        MenuEntry::Firmware {
            title: "Windows Boot Manager",
        },
        MenuEntry::Firmware {
            title: "Windows-Boot-Manager",
        },
    ];
    let posing = [MenuEntry::Firmware { title: "slopos a" }, SLOTS[0]];
    for entries in [&twice[..], &titles, &mapped, &posing] {
        assert_eq!(
            config(entries, "").render(&mut sink),
            Err(RenderError::Duplicate)
        );
    }
    let distinct = [SLOTS[0], MenuEntry::Firmware { title: "SlopOS-A" }];
    assert!(config(&distinct, "").render(&mut sink).is_ok());
}

fn firmware_entry(description: &str, path: &str, attributes: u32) -> Vec<u8> {
    let partition = HardDrive {
        partition_number: 1,
        start_lba: 2048,
        blocks: 2048,
        partition: BOOT,
    };
    let mut file_path_list = vec![0u8; device_path::loader_len(path)];
    device_path::loader(&partition, path, &mut file_path_list).unwrap();
    let mut out = vec![0u8; load_option::encoded_len(description, &file_path_list)];
    load_option::encode(attributes, description, &file_path_list, &mut out).unwrap();
    out
}

/// The title offered for `listed[index]`, `listed` being `BootOrder`'s
/// variables, `None` where none exists.
fn offered(
    listed: &[Option<Vec<u8>>],
    index: usize,
    own_fallback: Option<&Guid>,
) -> Option<String> {
    let listed: Vec<Listed<'_>> = listed
        .iter()
        .map(|raw| raw.as_deref().map_or(Listed::Absent, Listed::Value))
        .collect();
    let mut buf = [0u8; 127];
    offered_title(&listed, index, own_fallback, &mut buf).map(String::from)
}

const GRUB: &str = r"\EFI\cachyos\grubx64.efi";

#[test]
fn only_other_systems_active_installed_loaders_are_offered() {
    let cachyos = firmware_entry("cachyos", GRUB, ACTIVE);
    assert_eq!(
        offered(&[Some(cachyos)], 0, None).as_deref(),
        Some("cachyos")
    );
    let ours = firmware_entry("SlopOS", r"\efi\slopos\bootx64.efi", ACTIVE);
    assert_eq!(offered(&[Some(ours)], 0, None), None);
    for refused in [
        firmware_entry("cachyos", GRUB, 0),
        firmware_entry("caf\u{e9}", GRUB, ACTIVE),
        firmware_entry("${ARCH}", GRUB, ACTIVE),
        firmware_entry("SLOPOS", GRUB, ACTIVE),
    ] {
        assert_eq!(offered(&[Some(refused)], 0, None), None);
    }
}

/// Limine boots the first `BootOrder` option of a description, offered or
/// not, and reads a description from any variable long enough to hold one.
#[test]
fn only_the_first_option_limine_takes_for_a_description_is_offered() {
    let fedora = firmware_entry("fedora", r"\EFI\fedora\shimx64.efi", ACTIVE);
    let mut broken = firmware_entry("CachyOS", GRUB, ACTIVE);
    broken[4] = 0xFF;
    let listed = [
        Some(firmware_entry("CachyOS", r"\EFI\BOOT\BOOTX64.EFI", 0)),
        Some(firmware_entry("cachyos", GRUB, ACTIVE)),
        None,
        Some(fedora.clone()),
        Some(fedora),
    ];
    assert_eq!(offered(&listed, 1, None), None);
    assert_eq!(offered(&listed, 3, None).as_deref(), Some("fedora"));
    assert_eq!(offered(&listed, 4, None), None);
    let shadowed = [Some(broken), Some(firmware_entry("cachyos", GRUB, ACTIVE))];
    assert_eq!(offered(&shadowed, 1, None), None);
}

#[test]
fn the_removable_path_of_an_esp_slopos_created_is_its_own() {
    let fallback = firmware_entry("UEFI disk", r"\EFI\BOOT\BOOTX64.EFI", ACTIVE);
    assert_eq!(offered(&[Some(fallback.clone())], 0, Some(&BOOT)), None);
    assert_eq!(
        offered(&[Some(fallback)], 0, Some(&ROOT)).as_deref(),
        Some("UEFI disk")
    );
}

/// Limine reads 128 `BootOrder` numbers, whether or not a variable stands
/// behind each, and refuses every `efi_boot_entry` past that; SlopOS's own
/// entry joins `BootOrder` after the menu is written.
#[test]
fn nothing_is_offered_past_what_limine_reads_of_boot_order() {
    let cachyos = firmware_entry("cachyos", GRUB, ACTIVE);
    let ours = firmware_entry("SlopOS", r"\EFI\SlopOS\BOOTX64.EFI", ACTIVE);
    let mut listed: Vec<Option<Vec<u8>>> = vec![None; 127];
    listed[0] = Some(cachyos);
    assert_eq!(offered(&listed, 0, None).as_deref(), Some("cachyos"));
    listed.push(None);
    assert_eq!(offered(&listed, 0, None), None);
    listed[127] = Some(ours);
    assert_eq!(offered(&listed, 0, None).as_deref(), Some("cachyos"));
}

#[test]
fn an_unreadable_option_ahead_may_be_what_limine_boots() {
    let cachyos = firmware_entry("cachyos", GRUB, ACTIVE);
    let mut buf = [0u8; 127];
    let ahead = [Listed::Unreadable, Listed::Value(&cachyos)];
    assert_eq!(offered_title(&ahead, 1, None, &mut buf), None);
    let behind = [Listed::Value(&cachyos), Listed::Unreadable];
    assert_eq!(offered_title(&behind, 0, None, &mut buf), Some("cachyos"));
}

/// Titles can each be offered and still collide in the menu, which the
/// installer resolves by leaving out the later.
#[test]
fn offered_titles_that_collide_are_told_apart_before_rendering() {
    let slot = SLOTS[0];
    let paren = MenuEntry::Firmware {
        title: "Linux (6.1)",
    };
    let bracket = MenuEntry::Firmware {
        title: "Linux [6.1]",
    };
    let posing = MenuEntry::Firmware { title: "slopos a" };
    assert!(collide(&paren, &bracket));
    assert!(collide(&slot, &posing));
    assert!(!collide(&slot, &paren));
}
