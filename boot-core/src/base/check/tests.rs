use super::*;
use crate::base::{INTERFACE_PATH, ManifestLine};
use crate::newc::tests::pack;
use std::string::String;
use std::vec::Vec;

/// A stand-in for SHA-256: the check only compares what it is handed.
fn toy(data: &[u8]) -> Digest {
    let mut out = [0u8; 32];
    for (i, &b) in data.iter().enumerate() {
        out[i % 32] = out[i % 32].wrapping_mul(31).wrapping_add(b);
    }
    out[31] = data.len() as u8;
    out
}

type Rec<'a> = (&'a str, u32, &'a [u8]);

/// The manifest of `records`, leaving out a line the writer refuses, as
/// the check refuses its record first.
fn manifest_of(records: &[Rec<'_>]) -> String {
    let mut manifest = String::new();
    for &(path, mode, data) in records {
        let line = ManifestLine {
            digest: toy(data),
            mode,
            path,
        };
        let _ = line.write(&mut manifest);
    }
    manifest
}

fn sealed(records: &[Rec<'_>]) -> Vec<u8> {
    let manifest = manifest_of(records);
    let mut all = records.to_vec();
    all.push((MANIFEST_PATH, S_IFREG | 0o644, manifest.as_bytes()));
    pack(&all)
}

fn listed_as(records: &[Rec<'_>], listed: &[Rec<'_>]) -> Vec<u8> {
    let manifest = manifest_of(listed);
    let mut all = records.to_vec();
    all.push((MANIFEST_PATH, S_IFREG | 0o644, manifest.as_bytes()));
    pack(&all)
}

fn good() -> Vec<Rec<'static>> {
    std::vec![
        ("/usr", S_IFDIR | 0o755, b""),
        ("/sbin/init", S_IFREG | 0o755, b"\x7fELF init"),
        ("/bin/sh", S_IFLNK | 0o777, b"shell"),
        ("/tmp", S_IFDIR | 0o1777, b""),
        ("/etc/hostname", S_IFREG | 0o644, b"slop\n"),
        (BUILD_TAG_PATH, S_IFREG | 0o644, b"tag-7\n"),
        (INTERFACE_PATH, S_IFREG | 0o644, b"3\n"),
    ]
}

fn with(extra: Rec<'static>) -> Vec<u8> {
    let mut records = good();
    records.push(extra);
    sealed(&records)
}

fn leaked(path: String) -> &'static str {
    std::boxed::Box::leak(path.into_boxed_str())
}

#[test]
fn what_the_packer_writes_checks_out_with_its_interface_and_tag() {
    let image = sealed(&good());
    let checked = check(&image, toy).unwrap();
    assert_eq!(checked.interface, 3);
    assert_eq!(checked.build_tag, Some("tag-7"));
    assert!(
        checked
            .manifest
            .ends_with(b" 100644 /usr/share/slopos/base-interface\n")
    );
    let described = describe(&image);
    assert_eq!(
        (described.interface, described.manifest, described.build_tag),
        (Ok(3), Some(checked.manifest), Some("tag-7"))
    );
}

#[test]
fn a_name_the_kernel_reads_otherwise_is_refused() {
    for name in [
        "bin/x",
        "./bin/x",
        "/bin//x",
        "/bin/./x",
        "/opt/x/",
        "/usr/../etc/x",
        "/",
    ] {
        let image = with((leaked(name.into()), S_IFREG | 0o644, b""));
        assert_eq!(
            check(&image, toy).map(|_| ()),
            Err(CheckError::Name(name.as_bytes())),
            "{name:?}"
        );
    }
    let long = leaked(std::format!("/{}", "n".repeat(USER_NAME_MAX + 1)));
    let deep = leaked("/d".repeat(USER_PATH_MAX / 2));
    for name in [long, deep] {
        assert!(matches!(
            check(&with((name, S_IFREG | 0o644, b"")), toy),
            Err(CheckError::Name(_))
        ));
    }
    let fits = leaked("/x".repeat(130));
    assert!(check(&with((fits, S_IFREG | 0o644, b"")), toy).is_ok());
}

#[test]
fn what_the_kernel_would_index_mount_or_unpack_otherwise_is_refused() {
    let cases: [(Rec<'static>, &str); 11] = [
        (("/sbin/init", S_IFREG | 0o755, b"again"), "Twice"),
        (("/bin/sh/x", S_IFREG, b""), "BeneathFile"),
        (("/dev/null", 0o020_666, b""), "Kind"),
        (("/bin/empty", S_IFLNK | 0o777, b""), "Target"),
        (("/bin/nul", S_IFLNK | 0o777, b"\0shell"), "Target"),
        (("/lib", S_IFREG | 0o644, b""), "NotDirectory"),
        (("/etc/ssl", S_IFLNK | 0o777, b"/x"), "NotDirectory"),
        (("/run", S_IFREG | 0o644, b""), "NotDirectory"),
        (("/dev", S_IFLNK | 0o777, b"/x"), "NotDirectory"),
        (("/dev/console", S_IFREG | 0o644, b""), "InDevfs"),
        (("/dev", S_IFDIR | 0o755, b""), "ok"),
    ];
    for (extra, want) in cases {
        let image = with(extra);
        let name = match check(&image, toy) {
            Ok(_) => "ok",
            Err(CheckError::Twice(_)) => "Twice",
            Err(CheckError::BeneathFile(_)) => "BeneathFile",
            Err(CheckError::Kind(_)) => "Kind",
            Err(CheckError::Target(_)) => "Target",
            Err(CheckError::NotDirectory(_)) => "NotDirectory",
            Err(CheckError::InDevfs(_)) => "InDevfs",
            Err(e) => panic!("{e}"),
        };
        assert_eq!(name, want, "{}", extra.0);
    }
    let mut usr = good();
    usr.retain(|r| r.0 != "/usr");
    usr.push(("/usr", S_IFREG | 0o644, b""));
    assert_eq!(
        check(&sealed(&usr), toy).map(|_| ()),
        Err(CheckError::NotDirectory(b"/usr"))
    );
}

#[test]
fn a_file_beneath_one_packed_later_is_refused_too() {
    let mut records = good();
    records.insert(0, ("/sbin/init/x", S_IFREG, b""));
    assert_eq!(
        check(&sealed(&records), toy).map(|_| ()),
        Err(CheckError::BeneathFile(b"/sbin/init"))
    );
}

#[test]
fn a_manifest_that_misstates_the_records_is_refused() {
    assert_eq!(
        check(&pack(&good()), toy).map(|_| ()),
        Err(CheckError::NoManifest)
    );
    let image = sealed(&good());
    let tamper = |from: &[u8], to: &[u8]| {
        let mut bad = image.clone();
        let at = bad.windows(from.len()).position(|w| w == from).unwrap();
        bad[at..at + from.len()].copy_from_slice(to);
        bad
    };
    assert_eq!(
        check(&tamper(b"ELF init", b"ELF tini"), toy).map(|_| ()),
        Err(CheckError::Misstated("/sbin/init"))
    );
    assert_eq!(
        check(
            &tamper(b" 100755 /sbin/init\n", b" 100644 /sbin/init\n"),
            toy
        )
        .map(|_| ()),
        Err(CheckError::Misstated("/sbin/init"))
    );
    assert_eq!(
        check(&tamper(b" /bin/sh\n", b" /bin/zz\n"), toy).map(|_| ()),
        Err(CheckError::Unknown("/bin/zz"))
    );

    let records = good();
    let mut swapped = records.clone();
    swapped.swap(1, 2);
    assert_eq!(
        check(&listed_as(&records, &swapped), toy).map(|_| ()),
        Err(CheckError::Unknown("/bin/sh"))
    );
    let mut doubled = records.clone();
    doubled.insert(2, records[1]);
    assert_eq!(
        check(&listed_as(&records, &doubled), toy).map(|_| ()),
        Err(CheckError::Unknown("/sbin/init"))
    );
    assert_eq!(
        check(&listed_as(&records, &records[..6]), toy).map(|_| ()),
        Err(CheckError::Unlisted(INTERFACE_PATH.as_bytes()))
    );
    let mut extra = records.clone();
    extra.push(("/sbin/ghost", S_IFREG | 0o755, b""));
    assert_eq!(
        check(&listed_as(&records, &extra), toy).map(|_| ()),
        Err(CheckError::Unknown("/sbin/ghost"))
    );
}

#[test]
fn a_base_with_no_interface_or_too_many_records_is_refused() {
    let records: Vec<Rec<'_>> = good()
        .into_iter()
        .filter(|r| r.0 != INTERFACE_PATH)
        .collect();
    assert_eq!(
        check(&sealed(&records), toy).map(|_| ()),
        Err(CheckError::Interface(Error::NoInterface))
    );
    let names: Vec<String> = (0..MAX_RECORDS).map(|i| std::format!("/d{i}")).collect();
    let many: Vec<Rec<'_>> = names
        .iter()
        .map(|n| (n.as_str(), S_IFDIR | 0o755, &b""[..]))
        .collect();
    assert_eq!(
        check(&sealed(&many), toy).map(|_| ()),
        Err(CheckError::TooMany)
    );
}

#[test]
fn every_byte_flipped_is_checked_or_refused_without_a_panic() {
    let image = sealed(&good());
    for at in 0..image.len() {
        let mut flipped = image.clone();
        flipped[at] ^= 0x41;
        let _ = check(&flipped, toy);
        let _ = describe(&flipped);
    }
}
