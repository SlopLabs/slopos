use super::*;
use crate::newc::tests::pack;
use slopos_abi::fs::{S_IFDIR, S_IFLNK};
use std::string::String;
use std::vec::Vec;

fn base(interface: &[u8]) -> Vec<u8> {
    pack(&[
        ("/usr/share/slopos", S_IFDIR | 0o755, b""),
        ("/sbin/init", S_IFREG | 0o755, b"\x7fELF"),
        (INTERFACE_PATH, S_IFREG | 0o644, interface),
    ])
}

#[test]
fn the_interface_is_a_decimal_line_from_one() {
    for (text, want) in [
        (&b"1\n"[..], Ok(1)),
        (b"7", Ok(7)),
        (b"4294967295\n", Ok(u32::MAX)),
        (b"4294967296\n", Err(Error::BadInterface)),
        (b"0\n", Err(Error::BadInterface)),
        (b"01\n", Err(Error::BadInterface)),
        (b"", Err(Error::BadInterface)),
        (b"\n", Err(Error::BadInterface)),
        (b"1\n\n", Err(Error::BadInterface)),
        (b" 1\n", Err(Error::BadInterface)),
        (b"1 \n", Err(Error::BadInterface)),
        (b"-1\n", Err(Error::BadInterface)),
        (b"00000000001\n", Err(Error::BadInterface)),
    ] {
        assert_eq!(parse_interface(text), want, "{text:?}");
        assert_eq!(interface(&base(text)), want, "{text:?}");
    }
}

#[test]
fn a_base_without_its_interface_file_has_none() {
    let archive = pack(&[("/sbin/init", S_IFREG | 0o755, b"x")]);
    assert_eq!(interface(&archive), Err(Error::NoInterface));
    let linked = pack(&[(INTERFACE_PATH, S_IFLNK | 0o777, b"1")]);
    assert_eq!(interface(&linked), Err(Error::BadInterface));
    let cut = base(b"1\n");
    assert_eq!(
        interface(&cut[..cut.len() - 8]),
        Err(Error::Archive(newc::Error::Truncated))
    );
}

#[test]
fn the_interface_read_is_the_one_the_base_serves_the_last_record() {
    let mut archive = base(b"1\n");
    archive.truncate(archive.len() - 124);
    let mut later = pack(&[(INTERFACE_PATH, S_IFREG | 0o644, b"2\n")]);
    archive.append(&mut later);
    assert_eq!(interface(&archive), Ok(2));
}

#[test]
fn current_boots_when_served_and_previous_only_in_its_place() {
    let served = 2..=4;
    let i = Reading::Interface;
    let unreadable = Reading::Unreadable(Error::NoInterface);
    for (current, previous, want) in [
        (i(2), i(3), Some(Which::Current)),
        (i(4), i(9), Some(Which::Current)),
        (i(5), i(3), Some(Which::Previous)),
        (i(1), i(2), Some(Which::Previous)),
        (unreadable, i(2), Some(Which::Previous)),
        (Reading::Absent, i(2), Some(Which::Previous)),
        (i(3), Reading::Absent, Some(Which::Current)),
        (i(5), i(1), None),
        (unreadable, Reading::Absent, None),
        (Reading::Absent, Reading::Absent, None),
    ] {
        assert_eq!(
            choose(current, previous, served.clone()),
            want,
            "{current} / {previous}"
        );
    }
}

#[test]
fn a_reading_names_what_it_found() {
    let shown = |r: Reading| std::format!("{r}");
    assert_eq!(shown(Reading::of(Some(&base(b"3\n")))), "interface 3");
    assert_eq!(shown(Reading::of(None)), "absent");
    assert_eq!(
        shown(Reading::of(Some(b"junk"))),
        "not a newc archive (Truncated)"
    );
    assert_eq!(
        shown(Reading::of(Some(&base(b"x")))),
        "/usr/share/slopos/base-interface names no interface"
    );
}

#[test]
fn manifest_lines_read_back_as_written() {
    let lines = [
        ManifestLine {
            digest: [0xAB; 32],
            mode: 0o120777,
            path: "/bin/sh",
        },
        ManifestLine {
            digest: core::array::from_fn(|i| i as u8),
            mode: 0o040755,
            path: "/usr/share/a b",
        },
    ];
    let mut text = String::new();
    for line in &lines {
        line.write(&mut text).unwrap();
    }
    assert!(text.starts_with(&std::format!("{} 120777 /bin/sh\n", "ab".repeat(32))));
    let read: Vec<_> = manifest_lines(&text).map(Result::unwrap).collect();
    assert_eq!(read, lines);
}

#[test]
fn a_manifest_line_that_is_not_digest_mode_path_is_refused_where_it_stands() {
    let zero = "0".repeat(64);
    let good = std::format!("{zero} 100644 /bin/sh");
    for (text, line) in [
        (std::format!("{good}\nnope\n"), 2),
        (std::format!("{} 100644 /bin/sh\n", "0".repeat(63)), 1),
        (std::format!("{} 100644 /bin/sh\n", "A".repeat(64)), 1),
        (std::format!("{zero} /bin/sh\n"), 1),
        (std::format!("{zero} 10644 /bin/sh\n"), 1),
        (std::format!("{zero} 100648 /bin/sh\n"), 1),
        (std::format!("{zero} 100644 bin/sh\n"), 1),
        (std::format!("{zero} 100644 /bin/\tsh\n"), 1),
        (std::format!("{good}\n{zero} 100644  /x\n"), 2),
        (std::format!("{zero} 100644 /{}\n", "x".repeat(4096)), 1),
    ] {
        let first_bad = manifest_lines(&text).find_map(Result::err);
        assert_eq!(first_bad, Some(BadManifest { line }), "{text:?}");
    }
    let refused = ManifestLine {
        digest: [0; 32],
        mode: 0o100644,
        path: "/a\nb",
    };
    assert!(refused.write(&mut String::new()).is_err());
}
