use super::*;
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFREG};
use std::vec;
use std::vec::Vec;

/// An archive of `records` and the trailer.
pub(crate) fn pack(records: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let records = records.iter().map(|&(path, mode, data)| {
        Record::new(path.as_bytes(), mode, data).expect("a record that fits")
    });
    for record in records.chain([Record::trailer()]) {
        record
            .pieces()
            .iter()
            .for_each(|piece| out.extend_from_slice(piece));
    }
    out
}

fn sample() -> Vec<u8> {
    pack(&[
        ("/usr", S_IFDIR | 0o755, b""),
        ("/usr/share/a", S_IFREG | 0o644, b"hello"),
        ("/bin/sh", S_IFLNK | 0o777, b"shell"),
        ("./usr//share/./a", S_IFREG | 0o600, b"later"),
    ])
}

#[test]
fn records_read_back_as_written_up_to_the_trailer() {
    let archive = sample();
    assert_eq!(archive.len() % 4, 0);
    let read: Vec<(&[u8], u32, &[u8])> = entries(&archive)
        .map(|e| e.map(|e| (e.path, e.mode, e.data)).unwrap())
        .collect();
    assert_eq!(
        read,
        vec![
            (&b"/usr"[..], S_IFDIR | 0o755, &b""[..]),
            (b"/usr/share/a", S_IFREG | 0o644, b"hello"),
            (b"/bin/sh", S_IFLNK | 0o777, b"shell"),
            (b"./usr//share/./a", S_IFREG | 0o600, b"later"),
        ]
    );
}

#[test]
fn the_written_header_is_newc_with_one_link_and_upper_case_hex() {
    let archive = pack(&[("/x", S_IFREG | 0o755, b"abc")]);
    let header = core::str::from_utf8(&archive[..HEADER_LEN]).unwrap();
    let fields = [
        "00000000", "000081ED", "00000000", "00000000", "00000001", "00000000",
    ];
    let rest = [
        "00000003", "00000000", "00000000", "00000000", "00000000", "00000003",
    ];
    let expected: std::string::String = ["070701"]
        .into_iter()
        .chain(fields)
        .chain(rest)
        .chain(["00000000"])
        .collect();
    assert_eq!(header, expected);
    assert_eq!(&archive[HEADER_LEN..HEADER_LEN + 6], b"/x\0\0\0\0");
    assert_eq!(&archive[HEADER_LEN + 6..HEADER_LEN + 9], b"abc");
}

#[test]
fn a_path_is_found_in_any_spelling_and_the_last_record_wins() {
    let archive = sample();
    let found = find_last(&archive, "/usr/share/a").unwrap().unwrap();
    assert_eq!((found.data, found.mode), (&b"later"[..], S_IFREG | 0o600));
    assert_eq!(
        find_last(&archive, "usr/share").unwrap().map(|e| e.data),
        None
    );
    assert!(same_path(b"bin/sh", b"/bin//./sh"));
    assert!(!same_path(b"/bin/sh", b"/bin/s"));
}

#[test]
fn a_malformed_archive_is_an_error_wherever_it_breaks() {
    let archive = sample();
    for cut in [0, 6, HEADER_LEN, HEADER_LEN + 2, archive.len() - 4] {
        let last = entries(&archive[..cut]).last().unwrap();
        assert_eq!(last.map(|_| ()), Err(Error::Truncated), "cut at {cut}");
        assert_eq!(
            find_last(&archive[..cut], "/usr").map(|_| ()),
            Err(Error::Truncated)
        );
    }
    let mut bad = archive.clone();
    bad[0] = b'1';
    assert!(matches!(entries(&bad).next(), Some(Err(Error::BadMagic))));
    let mut bad = archive.clone();
    bad[FILESIZE_AT] = b'g';
    assert!(matches!(entries(&bad).next(), Some(Err(Error::BadField))));
    let mut bad = archive;
    bad[NAMESIZE_AT..NAMESIZE_AT + 8].copy_from_slice(b"00000000");
    assert!(matches!(entries(&bad).next(), Some(Err(Error::BadField))));
}

#[test]
fn a_size_past_the_archive_is_truncated_not_a_panic() {
    let mut archive = pack(&[("/x", S_IFREG, b"abc")]);
    archive[FILESIZE_AT..FILESIZE_AT + 8].copy_from_slice(b"FFFFFFFF");
    assert!(matches!(
        entries(&archive).next(),
        Some(Err(Error::Truncated))
    ));
    let mut archive = pack(&[("/x", S_IFREG, b"abc")]);
    archive[NAMESIZE_AT..NAMESIZE_AT + 8].copy_from_slice(b"FFFFFFFF");
    assert!(matches!(
        entries(&archive).next(),
        Some(Err(Error::Truncated))
    ));
}

#[test]
fn every_byte_flipped_is_read_or_refused_without_a_panic() {
    let archive = sample();
    for at in 0..archive.len() {
        let mut flipped = archive.clone();
        flipped[at] ^= 0x5A;
        let _ = entries(&flipped).count();
        let _ = find_last(&flipped, "/usr/share/a");
    }
}

#[test]
fn a_record_at_a_time_reads_what_the_whole_archive_reads() {
    let archive = sample();
    let mut at = 0;
    let mut read = Vec::new();
    loop {
        let found = read_header(&archive[at..]).unwrap();
        let name_at = at + HEADER_LEN;
        let name = &archive[name_at..name_at + found.name_size];
        let data_at = align4(name_at + found.name_size).unwrap();
        at = align4(data_at + found.size).unwrap();
        let Some(path) = path_of(name) else {
            break;
        };
        read.push((path, found.mode, &archive[data_at..data_at + found.size]));
    }
    assert_eq!(at, archive.len());
    let whole: Vec<(&[u8], u32, &[u8])> = entries(&archive)
        .map(|e| e.map(|e| (e.path, e.mode, e.data)).unwrap())
        .collect();
    assert_eq!(read, whole);
}

#[test]
fn a_header_is_refused_short_unmarked_or_nameless() {
    let archive = sample();
    for cut in 0..HEADER_LEN {
        assert_eq!(read_header(&archive[..cut]), Err(Error::Truncated), "{cut}");
    }
    let mut unmarked = archive.clone();
    unmarked[0] = b'1';
    assert_eq!(read_header(&unmarked), Err(Error::BadMagic));
    let mut nameless = archive;
    nameless[94..102].copy_from_slice(b"00000000");
    assert_eq!(read_header(&nameless), Err(Error::BadField));
}
