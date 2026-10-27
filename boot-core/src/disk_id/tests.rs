use super::*;
use std::string::String;

fn named(id: Option<DiskId>, alias: bool, partition: Option<u8>) -> Option<String> {
    let mut out = [0u8; DISK_ID_MAX];
    let len = id?.write(alias, partition, &mut out)?;
    Some(String::from_utf8(out[..len].to_vec()).unwrap())
}

fn name(id: Option<DiskId>) -> Option<String> {
    named(id, false, None)
}

#[test]
fn nvme_parts_keep_every_whitespace_byte_but_the_trailing() {
    let kingston = DiskId::nvme(b"  KINGSTON SNV3S1000G \t", b"50026B73\x0b 82A1\0 \0", 1);
    assert_eq!(
        name(kingston).as_deref(),
        Some("nvme-__KINGSTON_SNV3S1000G_50026B73__82A1_1")
    );
    assert_eq!(
        name(DiskId::nvme(b"a  b\x0b", b"s\x0c\r\n", 3)).as_deref(),
        Some("nvme-a__b__s__3"),
        "only space, tab, CR and LF trail away"
    );
}

#[test]
fn namespace_1_has_the_shorter_name_and_partitions_take_part_n() {
    let first = DiskId::nvme(b"QEMU NVMe Ctrl", b"slopos-root", 1);
    assert_eq!(
        name(first).as_deref(),
        Some("nvme-QEMU_NVMe_Ctrl_slopos-root_1")
    );
    assert_eq!(
        named(first, true, None).as_deref(),
        Some("nvme-QEMU_NVMe_Ctrl_slopos-root")
    );
    assert_eq!(
        named(first, false, Some(12)).as_deref(),
        Some("nvme-QEMU_NVMe_Ctrl_slopos-root_1-part12")
    );
    assert_eq!(
        named(first, true, Some(255)).as_deref(),
        Some("nvme-QEMU_NVMe_Ctrl_slopos-root-part255")
    );
    let second = DiskId::nvme(b"QEMU NVMe Ctrl", b"slopos-root", 2);
    assert_eq!(
        name(second).as_deref(),
        Some("nvme-QEMU_NVMe_Ctrl_slopos-root_2")
    );
    assert_eq!(named(second, true, None), None);
    assert_eq!(named(second, true, Some(1)), None);
}

#[test]
fn usb_and_virtio_parts_collapse_whitespace() {
    assert_eq!(
        name(DiskId::usb(
            b" Ven  dor\x0b",
            b"Pro\t\nduct",
            Some(b"\x0c A  B "),
            2
        ))
        .as_deref(),
        Some("usb-Ven_dor_Pro_duct_A_B-0:2")
    );
    assert_eq!(
        name(DiskId::usb(
            b"AB  CD  ",
            b"QEMU HARDDISK   ",
            Some(b"slopos-usb"),
            0
        ))
        .as_deref(),
        Some("usb-AB_CD_QEMU_HARDDISK_slopos-usb-0:0")
    );
    assert_eq!(
        name(DiskId::virtio(b" disk/1  caf\xc3\xa9\xff,\x0b")).as_deref(),
        Some("virtio-disk_1_caf\u{e9}__"),
        "valid UTF-8 kept, `/`, an invalid byte and `,` each `_`"
    );
}

#[test]
fn noncharacters_are_a_byte_each() {
    assert_eq!(
        name(DiskId::virtio("a\u{fdd0}b\u{fffe}c\u{1ffff}d".as_bytes())).as_deref(),
        Some("virtio-a___b___c____d")
    );
    assert_eq!(
        name(DiskId::virtio("\u{fdcf}\u{fdf0}\u{fffd}".as_bytes())).as_deref(),
        Some("virtio-\u{fdcf}\u{fdf0}\u{fffd}")
    );
}

#[test]
fn a_blank_usb_vendor_or_product_is_an_empty_part() {
    assert_eq!(
        name(DiskId::usb(b"  ", b"Prod", Some(b"S"), 0)).as_deref(),
        Some("usb-_Prod_S-0:0")
    );
    assert_eq!(
        name(DiskId::usb(b"Ven", b"", None, 0)).as_deref(),
        Some("usb-Ven_-0:0")
    );
}

#[test]
fn a_stick_without_a_serial_names_none() {
    for serial in [None, Some(&b""[..]), Some(b"  "), Some(b"\0x")] {
        assert_eq!(
            name(DiskId::usb(b"Ven", b"Prod", serial, 0)).as_deref(),
            Some("usb-Ven_Prod-0:0"),
            "{serial:?}"
        );
    }
}

#[test]
fn an_empty_nvme_or_virtio_part_names_no_disk() {
    assert!(DiskId::nvme(b"   ", b"serial", 1).is_none());
    assert!(DiskId::nvme(b"model", b"\0\0", 1).is_none());
    assert!(DiskId::nvme(b"\0model", b"serial", 1).is_none());
    assert!(DiskId::virtio(b"").is_none());
    assert!(DiskId::virtio(b" \x0b ").is_none());
}

#[test]
fn a_name_fills_its_stem_to_the_byte() {
    const STEM: usize = DISK_ID_MAX - 8;
    let longest = [b'v'; STEM - 7];
    let id = DiskId::virtio(&longest);
    assert_eq!(name(id).map(|n| n.len()), Some(STEM));
    assert_eq!(
        named(id, false, Some(255)).map(|n| n.len()),
        Some(DISK_ID_MAX)
    );
    assert!(DiskId::virtio(&[b'v'; STEM - 6]).is_none());
    let serial = [b'S'; 126];
    let stick = DiskId::usb(b"VENDOR12", b"PRODUCT123456789", Some(&serial), 15);
    assert!(named(stick, false, Some(255)).is_some_and(|n| n.ends_with("SSS-0:15-part255")));
    let mut short = [0u8; 8];
    assert_eq!(stick.unwrap().write(false, None, &mut short), None);
}

#[test]
fn kept_is_udevs_whitelist() {
    const ACCEPTED: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#+-.:=@_";
    assert_eq!(ACCEPTED.len(), 70);
    for c in (0..0x80u8).map(char::from) {
        assert_eq!(kept(c), ACCEPTED.contains(c), "{c:?}");
    }
    for c in ['/', '\\', ',', '$', '%', '?', '!', ' '] {
        assert!(!kept(c), "{c:?}");
    }
    for c in ['\u{e9}', '\u{fdcf}', '\u{fdf0}', '\u{fffd}', '\u{1f600}'] {
        assert!(kept(c), "{c:?}");
    }
    for c in [
        '\u{fdd0}',
        '\u{fdef}',
        '\u{fffe}',
        '\u{ffff}',
        '\u{1fffe}',
        '\u{10ffff}',
    ] {
        assert!(!kept(c), "{c:?}");
    }
}
