use super::*;
use alloc::vec;
use slopos_abi::fs::S_IFLNK;

pub(crate) const RELEASE_TEXT: &str = "name hello\nversion 3\nsummary Says hi\n";
pub(crate) const DECLARE_TEXT: &str = "\
user
command hello
app hello
service hellod
  program /opt/hello/bin/hellod
  reads   /opt/hello/share/
";

pub(crate) fn packer() -> Packer {
    let mut packer = Packer::default();
    packer.add("/bin", S_IFDIR | 0o755, Vec::new());
    packer.add("/bin/hello", S_IFREG | 0o755, b"hello".to_vec());
    packer.add("/bin/hellod", S_IFREG | 0o755, b"hellod".to_vec());
    packer.add(
        "/share/slopos/apps/hello.app",
        S_IFREG | 0o644,
        b"id=hello\n".to_vec(),
    );
    packer.add("/share/link", S_IFLNK | 0o777, b"slopos/apps".to_vec());
    packer
}

#[test]
fn what_the_packer_writes_checks_out() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let image = Image::check(&bytes).unwrap();
    assert_eq!(
        (image.release.name.as_str(), image.release.version),
        ("hello", 3)
    );
    assert_eq!(image.declaration.commands, ["hello"]);
    assert!(image.manifest.ends_with(b" 100644 /.extension/declare\n"));
    assert_eq!(
        image.links().collect::<Vec<_>>(),
        [("/share/link", "slopos/apps")]
    );
    let files: Vec<&str> = image.files().map(|(path, _)| path).collect();
    assert_eq!(
        files,
        [
            "/bin/hello",
            "/bin/hellod",
            "/share/slopos/apps/hello.app",
            RELEASE,
            DECLARE,
            MANIFEST
        ]
    );
}

fn refusal(packer: Packer, release: &str, declare: &str) -> String {
    packer.finish(release, declare).unwrap_err()
}

#[test]
fn an_image_out_of_shape_is_refused_saying_why() {
    let mut outside = packer();
    outside.add("/etc/passwd", S_IFREG | 0o644, b"root".to_vec());
    assert!(refusal(outside, RELEASE_TEXT, DECLARE_TEXT).contains("/etc/passwd: outside"));
    let mut extra = packer();
    extra.add("/.extension/hook", S_IFREG | 0o755, b"x".to_vec());
    assert!(refusal(extra, RELEASE_TEXT, DECLARE_TEXT).contains("/.extension/hook"));
    let mut twice = packer();
    twice.add("/bin/hello", S_IFREG | 0o755, b"again".to_vec());
    assert!(refusal(twice, RELEASE_TEXT, DECLARE_TEXT).contains("packed twice"));
    assert!(refusal(packer(), "name bin\nversion 1\n", "").contains(RELEASE));
    assert!(refusal(packer(), RELEASE_TEXT, "frobnicate\n").contains(DECLARE));
    for (declare, missing) in [
        ("command ghost\n", "/opt/hello/bin/ghost"),
        ("app ghost\n", "/share/slopos/apps/ghost.app"),
        (
            "user\nservice s\n  program /opt/hello/share/link\n",
            "/opt/hello/share/link",
        ),
        (
            "user\nservice s\n  program /opt/hello/bin/hellod\n  reads /opt/hello/etc/\n",
            "/opt/hello/etc/",
        ),
    ] {
        let why = refusal(packer(), RELEASE_TEXT, declare);
        assert!(why.contains(missing), "{declare:?}: {why}");
    }
    for target in ["../bin/hello", "/usr/local/lib", "a/../../x", ""] {
        let mut escapes = packer();
        escapes.add("/lib", S_IFLNK | 0o777, target.as_bytes().to_vec());
        let why = refusal(escapes, RELEASE_TEXT, DECLARE_TEXT);
        assert!(why.contains("/lib: a symlink"), "{target:?}: {why}");
    }
    let mut plain = packer();
    plain.add("/bin/plain", S_IFREG | 0o644, b"x".to_vec());
    assert!(refusal(plain, RELEASE_TEXT, "command plain\n").contains("/opt/hello/bin/plain"));
}

#[test]
fn a_record_not_as_the_manifest_says_is_refused() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let mut bent = Vec::new();
    for entry in newc::entries(&bytes).flatten() {
        let data: &[u8] = if entry.path == b"/bin/hellod" {
            b"hellox"
        } else {
            entry.data
        };
        let record = Record::new(entry.path, entry.mode, data).unwrap();
        record
            .pieces()
            .iter()
            .for_each(|piece| bent.extend_from_slice(piece));
    }
    Record::trailer()
        .pieces()
        .iter()
        .for_each(|piece| bent.extend_from_slice(piece));
    let why = Image::check(&bent).unwrap_err().to_string();
    assert!(why.contains("not what /.extension/manifest says"), "{why}");
    assert!(Image::check(&bytes[..bytes.len() / 2]).is_err());
    assert!(Image::check(&vec![0u8; 512]).is_err());
}
