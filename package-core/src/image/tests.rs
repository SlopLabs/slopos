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
    extra.add("/.package/hook", S_IFREG | 0o755, b"x".to_vec());
    assert!(refusal(extra, RELEASE_TEXT, DECLARE_TEXT).contains("/.package/hook"));
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
    for target in ["../bin/hello", "/usr/lib", "a/../../x", ""] {
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
        why.contains("/bin/hellod: not what /.package/manifest says"),
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

    fn cores(&self) -> usize {
        self.1
    }
}

/// Puts past `fails_after` fail.
struct Handed {
    out: Vec<u8>,
    puts: usize,
    made: usize,
    spare: Vec<Vec<u8>>,
    fails_after: usize,
}

impl Handed {
    fn new(len: usize) -> Self {
        Self {
            out: vec![0; len],
            puts: 0,
            made: 0,
            spare: Vec::new(),
            fails_after: usize::MAX,
        }
    }
}

impl Sink for Handed {
    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        Ok(self.spare.pop().unwrap_or_else(|| {
            self.made += 1;
            Vec::new()
        }))
    }

    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), String> {
        if self.puts == self.fails_after {
            return Err("the disk is full".into());
        }
        self.puts += 1;
        assert!(bytes.len() % BLOCK == 0);
        for run in runs(&bytes) {
            assert!(
                bytes[run.clone()]
                    .chunks(BLOCK)
                    .all(|b| b.iter().any(|&x| x != 0))
            );
            self.out[at as usize + run.start..][..run.len()].copy_from_slice(&bytes[run]);
        }
        self.spare.push(bytes);
        Ok(())
    }
}

fn copied_to(
    source: &[u8],
    digest: &Digest,
    threads: usize,
    mut sink: Handed,
) -> (Result<(), ImageError>, Handed) {
    let copied = copy(
        &mut Cores(source, threads),
        source.len() as u64,
        digest,
        &mut sink,
        &mut |_, _| {},
    );
    (copied, sink)
}

fn copied(source: &[u8], digest: &Digest, threads: usize) -> (Result<(), ImageError>, Handed) {
    copied_to(source, digest, threads, Handed::new(source.len()))
}

#[test]
fn a_sink_that_fails_ends_the_copy_with_its_failure() {
    let mut packer = packer();
    let big: Vec<u8> = (0..6 * COPY_CHUNK).map(|i| (i % 251) as u8 | 1).collect();
    packer.add("/share/big", S_IFREG | 0o644, big);
    let bytes = packer.finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let (_, root) = root_of(&mut &bytes[..]).unwrap();
    for fails_after in [0, 2] {
        let mut sink = Handed::new(bytes.len());
        sink.fails_after = fails_after;
        let (copied, _) = copied_to(&bytes, &root, 2, sink);
        let why = copied.unwrap_err().to_string();
        assert!(why.contains("the disk is full"), "{why}");
    }
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
        let (copied, sink) = copied(&bytes, &root, threads);
        assert_eq!(copied, Ok(()));
        assert!(sink.out == bytes, "{threads}");
        let most = bytes.len().div_ceil(COPY_CHUNK) + 2;
        assert!(sink.puts <= most, "{} puts, {most} at most", sink.puts);
        let most = READ_AHEAD + 2;
        assert!(
            sink.made <= most,
            "{} buffers made, {most} at most",
            sink.made
        );
    }
    let mut other = root;
    other[5] ^= 1;
    assert!(matches!(
        copied(&bytes, &other, 2).0,
        Err(ImageError::Damaged(_))
    ));
    let mut flipped = bytes.clone();
    flipped[2 * BLOCK + 1] ^= 4;
    assert!(matches!(
        copied(&flipped, &root, 2).0,
        Err(ImageError::Damaged(_))
    ));
    assert!(matches!(
        copied(&bytes[..bytes.len() - 1], &root, 1).0,
        Err(ImageError::Format(_))
    ));
}

#[test]
fn a_large_tree_is_copied_and_held_a_chunk_of_leaves_at_a_time() {
    let blocks = 128 * 128 + 300;
    let geometry = Geometry::new(blocks as u64).unwrap();
    let mut bytes = vec![0; geometry.image_bytes() as usize];
    for (k, block) in bytes[..blocks * BLOCK].chunks_mut(BLOCK).enumerate() {
        if k % 5 != 0 {
            block[..8].copy_from_slice(&(k as u64 + 1).to_le_bytes());
        }
    }
    let (data, tree) = bytes.split_at_mut(blocks * BLOCK);
    let mut builder = verity::Builder::new(geometry, tree).unwrap();
    for block in data.chunks_exact(BLOCK) {
        builder.push(block.try_into().unwrap());
    }
    let root = builder.finish().unwrap();
    assert!(geometry.levels() >= 3);
    let (copied, sink) = copied(&bytes, &root, 2);
    assert_eq!(copied, Ok(()));
    assert!(sink.out == bytes);
    assert_eq!(verify_tree(&mut &bytes[..], &root), Ok(()));
    let (start, _) = geometry.level(0).unwrap();
    let mut flipped = bytes.clone();
    flipped[(start as usize + 1) * BLOCK + 70] ^= 2;
    assert!(verify_tree(&mut &flipped[..], &root).is_err());
}

#[test]
fn a_run_is_every_block_between_two_of_zeros() {
    let mut bytes = vec![0; 6 * BLOCK];
    for k in [1, 2, 4] {
        bytes[k * BLOCK + 3] = 1;
    }
    let found: Vec<_> = runs(&bytes).collect();
    assert_eq!(found, [BLOCK..3 * BLOCK, 4 * BLOCK..5 * BLOCK]);
    assert_eq!(runs(&vec![0; 2 * BLOCK]).count(), 0);
    assert_eq!(runs(&[]).count(), 0);
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

/// The copy's answer, the contents check's, the sink, and the reads each took.
fn copied_whole(
    bytes: &[u8],
) -> (
    Result<(), ImageError>,
    Result<(), ImageError>,
    Handed,
    usize,
    usize,
) {
    let mut image = Image::open(Counted { bytes, reads: 0 }, None).unwrap();
    let mut sink = Handed::new(bytes.len());
    let opened = image.source().reads;
    let copied = image.copy(&mut sink);
    let read = image.source().reads - opened;
    let checked = image.check_contents();
    let after = image.source().reads - opened - read;
    (copied, checked, sink, read, after)
}

#[test]
fn a_copy_holds_the_files_to_the_manifest_in_its_one_read() {
    let mut packer = packer();
    let big: Vec<u8> = (0..3 * COPY_CHUNK + 77)
        .map(|i| (i % 249) as u8 | 1)
        .collect();
    packer.add("/share/big", S_IFREG | 0o644, big);
    packer.add("/share/tail", S_IFREG | 0o644, vec![7; 5]);
    let bytes = packer.finish(RELEASE_TEXT, DECLARE_TEXT).unwrap();
    let (copied, checked, sink, read, after) = copied_whole(&bytes);
    assert_eq!(copied, Ok(()));
    assert_eq!(checked, Ok(()));
    assert!(sink.out == bytes);
    let (geometry, _) = root_of(&mut &bytes[..]).unwrap();
    let chunks = (geometry.data_blocks() as usize * BLOCK).div_ceil(COPY_CHUNK);
    assert!(read <= chunks + 2, "{read} reads for {chunks} chunks");
    assert_eq!(after, 0, "the contents check reads nothing a copy read");

    let bent = rebuilt(&bytes, b"hellod", b"hellox");
    let (copied, checked, sink, _, _) = copied_whole(&bent);
    assert_eq!(copied, Ok(()), "the bytes are the tree's");
    assert!(sink.out == bent);
    let why = checked.unwrap_err().to_string();
    assert!(why.contains("/bin/hellod: not what"), "{why}");
}

#[test]
fn a_passing_file_is_hashed_with_its_holes_and_only_in_order() {
    let gathered = || Ok(Vec::new());
    let want = |bytes: &[u8]| sha256(bytes);
    let mut whole = vec![0u8; 3 * BLOCK + 10];
    whole[..4].copy_from_slice(b"text");
    whole[2 * BLOCK..2 * BLOCK + 4].copy_from_slice(b"more");
    let mut file = Passing::new(
        "/f".into(),
        12,
        want(&whole),
        whole.len() as u64,
        compress_blocks,
    );
    file.feed(0, &whole[..BLOCK]);
    file.feed(2 * BLOCK as u64, &whole[2 * BLOCK..3 * BLOCK]);
    let mut g = gathered();
    assert_eq!(
        file.finish(&mut g),
        Some(want(&whole)),
        "holes read as zeros"
    );
    assert!(g.is_ok_and(|g| g.is_empty()));

    let mut late = Passing::new(
        "/f".into(),
        12,
        want(&whole),
        whole.len() as u64,
        compress_blocks,
    );
    late.feed(2 * BLOCK as u64, &whole[2 * BLOCK..3 * BLOCK]);
    late.feed(0, &whole[..BLOCK]);
    assert_eq!(
        late.finish(&mut gathered()),
        None,
        "a piece before what was fed"
    );

    let mut past = Passing::new("/f".into(), 12, want(b"ab"), 2, compress_blocks);
    past.feed(0, &[b'a', b'b', 9, 9]);
    past.feed(BLOCK as u64, &[1; BLOCK]);
    assert_eq!(
        past.finish(&mut gathered()),
        Some(want(b"ab")),
        "only its size"
    );
}
