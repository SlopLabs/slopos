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
    let mut image = Image::check(&bytes[..]).unwrap();
    assert_eq!(
        (image.release.name.as_str(), image.release.version),
        ("hello", 3)
    );
    assert_eq!(image.declaration.commands, ["hello"]);
    assert_eq!(image.digest, root_of(&mut &bytes[..]).unwrap().1);
    assert!(
        image
            .manifest
            .ends_with(b" 100644 /share/slopos/apps/hello.app\n")
    );
    assert_eq!(
        image.links().collect::<Vec<_>>(),
        [("/share/link", "slopos/apps")]
    );
    let files: Vec<&str> = image.files().map(|f| f.path.as_str()).collect();
    assert_eq!(
        files,
        [
            DECLARE,
            MANIFEST,
            RELEASE,
            "/bin/hello",
            "/bin/hellod",
            "/share/slopos/apps/hello.app",
        ]
    );
    assert_eq!(image.read("/bin/hellod", 64).unwrap(), b"hellod");
    assert_eq!(
        image.entry("/share/slopos").map(|e| e.mode),
        Some(0o040755),
        "a directory a path implies is packed with mode 0755"
    );
    assert!(packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap() == bytes);
}

fn refusal(packer: Packer, release: &str, declare: &str) -> String {
    packer.finish(release, declare).unwrap_err()
}

#[test]
fn an_image_out_of_shape_is_refused_saying_why() {
    let mut outside = packer();
    outside.add("/etc/passwd", S_IFREG | 0o644, b"root".to_vec());
    assert!(refusal(outside, RELEASE_TEXT, DECLARE_TEXT).contains("/etc: outside"));
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
fn a_command_may_be_a_link_to_an_executable_file_of_the_image() {
    let mut linked = packer();
    linked.add("/bin/hi", S_IFLNK | 0o777, b"./hello".to_vec());
    linked.add("/bin/hi++", S_IFLNK | 0o777, b"hi".to_vec());
    linked.add("/bin/tool/run", S_IFREG | 0o755, b"run".to_vec());
    linked.add("/bin/tools", S_IFLNK | 0o777, b"tool".to_vec());
    linked.add("/bin/run", S_IFLNK | 0o777, b"tools//run".to_vec());
    let bytes = linked
        .finish(RELEASE_TEXT, "command hi\ncommand hi++\ncommand run\n")
        .unwrap();
    let image = Image::check(&bytes[..]).unwrap();
    assert_eq!(image.declaration.commands, ["hi", "hi++", "run"]);

    for (links, why) in [
        (&[("/bin/plain", "hellox")][..], "a link to nothing"),
        (
            &[("/bin/plain", "tool/data")][..],
            "a link to a file no one runs",
        ),
        (&[("/bin/plain", "tool")][..], "a link to a directory"),
        (
            &[("/bin/plain", "a"), ("/bin/a", "b"), ("/bin/b", "plain")][..],
            "a loop",
        ),
    ] {
        let mut refused = packer();
        refused.add("/bin/tool/data", S_IFREG | 0o644, b"x".to_vec());
        for (at, target) in links {
            refused.add(at, S_IFLNK | 0o777, target.as_bytes().to_vec());
        }
        let said = refusal(refused, RELEASE_TEXT, "command plain\n");
        assert!(
            said.contains("/opt/hello/bin/plain"),
            "{why} is refused as a command: {said}"
        );
    }

    let chain = |links: usize| {
        let mut chained = packer();
        let mut prev = String::from("hello");
        for hop in 0..links {
            chained.add(&format!("/bin/h{hop}"), S_IFLNK | 0o777, prev.into_bytes());
            prev = format!("h{hop}");
        }
        chained.finish(RELEASE_TEXT, &format!("command {prev}\n"))
    };
    assert!(chain(LINK_HOPS - 1).is_ok());
    let said = chain(LINK_HOPS).unwrap_err();
    assert!(said.contains(&format!("/bin/h{}", LINK_HOPS - 1)), "{said}");
}

/// `bytes` with its data's block holding `from` holding `to` instead, its
/// tree built again so it vouches for the change.
fn rebuilt(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let (geometry, _) = root_of(&mut &bytes[..]).unwrap();
    let data = geometry.data_blocks() as usize * BLOCK;
    let mut data = bytes[..data].to_vec();
    let at = data
        .chunks_exact(BLOCK)
        .position(|block| block.starts_with(from) && block[from.len()..].iter().all(|&b| b == 0))
        .unwrap()
        * BLOCK;
    data[at..at + to.len()].copy_from_slice(to);
    let mut tree = vec![0; geometry.tree_blocks() as usize * BLOCK];
    let mut builder = verity::Builder::new(geometry, &mut tree).unwrap();
    for block in data.chunks_exact(BLOCK) {
        builder.push(block.try_into().unwrap());
    }
    builder.finish().unwrap();
    data.extend_from_slice(&tree);
    data
}

#[test]
fn a_file_not_as_the_manifest_says_is_refused() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let bent = rebuilt(&bytes, b"hellod", b"hellox");
    let why = Image::check(&bent[..]).unwrap_err().to_string();
    assert!(
        why.contains("/bin/hellod: not what /.extension/manifest says"),
        "{why}"
    );
    assert!(
        Image::open(&bent[..], None).is_ok(),
        "opening holds the tree to the manifest, not each file's bytes"
    );
    assert_eq!(
        Image::open(&bytes[..], Some([0; 32])).unwrap_err(),
        ImageError::Other
    );
}

#[test]
fn a_flipped_bit_fails_the_read_that_meets_it() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let at = bytes
        .chunks_exact(BLOCK)
        .position(|block| block.starts_with(b"hellod\0"))
        .unwrap()
        * BLOCK;
    let mut flipped = bytes.clone();
    flipped[at] ^= 0x40;
    let mut image = Image::open(&flipped[..], None).unwrap();
    assert!(image.read("/bin/hello", 64).is_ok());
    let why = image.read("/bin/hellod", 64).unwrap_err().to_string();
    assert!(
        why.contains("does not hold what the image's tree says"),
        "{why}"
    );
    assert!(Image::check(&flipped[..]).is_err());
}

#[test]
fn what_is_no_image_is_refused_for_what_it_is() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    assert!(Image::check(&bytes[..bytes.len() - BLOCK]).is_err());
    assert!(matches!(
        Image::check(&[0u8; 512][..]),
        Err(ImageError::Format(_))
    ));
}

#[test]
fn a_symlink_naming_a_nul_is_refused() {
    let mut packer = packer();
    packer.add("/lib/libz.so", S_IFLNK | 0o777, b"libz\0.so".to_vec());
    assert!(packer.finish(RELEASE_TEXT, DECLARE_TEXT).is_err());
}

#[test]
fn a_tree_is_held_to_its_root_and_its_data_left_to_the_readers() {
    let bytes = packer().finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let (geometry, root) = root_of(&mut &bytes[..]).unwrap();
    assert_eq!(verify_tree(&mut &bytes[..], &root), Ok(()));
    let mut other = root;
    other[0] ^= 1;
    assert_eq!(verify_tree(&mut &bytes[..], &other), Err(ImageError::Other));
    let leaf = geometry.image_blocks() as usize - 1;
    let mut flipped = bytes.clone();
    flipped[leaf * BLOCK + 20] ^= 1;
    assert!(verify_tree(&mut &flipped[..], &root).is_err());
    let mut flipped = bytes.clone();
    flipped[BLOCK + 2000] ^= 1;
    assert_eq!(verify_tree(&mut &flipped[..], &root), Ok(()));
    assert!(Image::check(&flipped[..]).is_err());
}

/// Bytes hashed on `threads` cores.
struct Cores<'a>(&'a [u8], usize);

impl Source for Cores<'_> {
    type Error = &'static str;

    fn len(&mut self) -> Result<u64, &'static str> {
        Ok(self.0.len() as u64)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), &'static str> {
        (&mut &self.0[..]).read_at(offset, out)
    }

    fn hash_blocks(&self, data: &[u8], out: &mut [Digest]) {
        hash_blocks(compress_blocks, data, out, self.1);
    }
}

/// An image's bytes as `copy` hands them over, a hole where it hands none.
fn copied(
    source: &[u8],
    digest: &Digest,
    threads: usize,
) -> (Result<u64, ImageError>, Vec<u8>, usize) {
    let mut out = vec![0; source.len()];
    let mut runs = 0;
    let copied = copy(&mut Cores(source, threads), digest, &mut |at, run| {
        runs += 1;
        assert!(run.len() % BLOCK == 0 && run.chunks(BLOCK).all(|b| b.iter().any(|&x| x != 0)));
        out[at as usize..][..run.len()].copy_from_slice(run);
        Ok(())
    });
    (copied, out, runs)
}

#[test]
fn a_copy_is_the_image_its_root_names_its_zeros_left_holes() {
    let mut packer = packer();
    let big: Vec<u8> = (0..COPY_CHUNK + 5000)
        .map(|i| (i % 251) as u8 | 1)
        .collect();
    packer.add("/share/big", S_IFREG | 0o644, big);
    let bytes = packer.finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let (_, root) = root_of(&mut &bytes[..]).unwrap();
    for threads in [1, 3] {
        let (copied, out, runs) = copied(&bytes, &root, threads);
        assert_eq!(copied, Ok(bytes.len() as u64));
        assert!(out == bytes, "{threads}");
        assert!(runs < 32, "{runs} runs");
    }
    let mut other = root;
    other[5] ^= 1;
    assert_eq!(copied(&bytes, &other, 2).0, Err(ImageError::Other));
    let mut flipped = bytes.clone();
    flipped[2 * BLOCK + 1] ^= 4;
    assert_eq!(copied(&flipped, &root, 2).0, Err(ImageError::Other));
    assert!(matches!(
        copied(&bytes[..bytes.len() - 1], &root, 1).0,
        Err(ImageError::Format(_))
    ));
}

/// Bytes read through a counter of calls.
struct Counted<'a> {
    bytes: &'a [u8],
    reads: usize,
}

impl Source for Counted<'_> {
    type Error = &'static str;

    fn len(&mut self) -> Result<u64, &'static str> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), &'static str> {
        self.reads += 1;
        (&mut &self.bytes[..]).read_at(offset, out)
    }
}

#[test]
fn a_file_is_read_in_runs_and_its_symbols_with_its_digest() {
    let mut packer = packer();
    let big: Vec<u8> = (0..2 * CHUNK).map(|i| (i % 253) as u8).collect();
    packer.add("/share/big", S_IFREG | 0o644, big.clone());
    let bytes = packer.finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let mut image = Image::open(
        Counted {
            bytes: &bytes,
            reads: 0,
        },
        None,
    )
    .unwrap();
    let opened = image.source().reads;
    image.check_contents().unwrap();
    let checked = image.source().reads - opened;
    assert!(
        checked < 40,
        "{checked} reads hold {} blocks to the manifest",
        bytes.len() / BLOCK
    );
    assert_eq!(image.read("/share/big", 4 * CHUNK as u64).unwrap(), big);
    let read = image.source().reads;
    assert!(image.take_gathered().is_some_and(|g| g.is_ok()));
    assert!(
        image.take_gathered().is_none(),
        "the symbol check takes it once"
    );
    assert_eq!(image.source().reads, read);
}
