use std::vec;
use std::vec::Vec;

use super::*;

fn hex(bytes: &[u8]) -> std::string::String {
    bytes.iter().map(|b| std::format!("{b:02x}")).collect()
}

#[test]
fn sha256_matches_the_fips_examples() {
    assert_eq!(
        hex(&sha256::digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256::digest(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256::digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    let mut split = sha256::Sha256::new();
    for _ in 0..1000 {
        split.update(&[b'a'; 1000]);
    }
    assert_eq!(
        hex(&split.finish()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn a_zero_block_hashes_as_sha256_does() {
    assert_eq!(hash(&[0; BLOCK]), sha256::digest(&[0; BLOCK]));
}

#[test]
fn levels_hold_128_digests_a_block_top_first() {
    let shape = |n| {
        let g = Geometry::new(n).unwrap();
        let blocks: Vec<u64> = (0..g.levels).map(|l| blocks_at(n, l)).collect();
        (g.levels, blocks, g.tree_blocks)
    };
    assert_eq!(shape(1), (0, vec![], 0));
    assert_eq!(shape(2), (1, vec![1], 1));
    assert_eq!(shape(128), (1, vec![1], 1));
    assert_eq!(shape(129), (2, vec![2, 1], 3));
    assert_eq!(shape(16384), (2, vec![128, 1], 129));
    assert_eq!(shape(16385), (3, vec![129, 2, 1], 132));
    let g = Geometry::new(16385).unwrap();
    assert_eq!(g.top(), 16385);
    assert_eq!(g.digest_at(2, 1), Some((16385, 32)));
    assert_eq!(g.digest_at(1, 128), Some((16385 + 1 + 1, 0)));
    assert_eq!(g.digest_at(0, 16384), Some((16385 + 3 + 128, 0)));
    assert_eq!(g.digest_at(3, 0), None);
}

#[test]
fn an_image_length_names_one_geometry() {
    for n in (1..3000).chain([16383, 16384, 16385, 1 << 20, MAX_DATA_BLOCKS]) {
        let g = Geometry::new(n).unwrap();
        assert_eq!(Geometry::of_image(g.image_bytes()), Some(g), "{n}");
    }
    for bytes in [0, 1, 4095, 4097, 130 * 4096, 131 * 4096] {
        assert_eq!(Geometry::of_image(bytes), None, "{bytes}");
    }
    assert_eq!(Geometry::new(0), None);
    assert_eq!(Geometry::new(MAX_DATA_BLOCKS + 1), None);
}

struct Image {
    blocks: Vec<u8>,
    verified: Vec<(u64, [u8; BLOCK])>,
    reads: usize,
    lookups: usize,
}

impl Source for Image {
    type Error = ();

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), ()> {
        self.reads += 1;
        let at = block as usize * BLOCK;
        out.copy_from_slice(self.blocks.get(at..at + BLOCK).ok_or(())?);
        Ok(())
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        self.lookups += 1;
        self.verified
            .iter()
            .find(|(b, _)| *b == block)
            .map(|(_, data)| out.copy_from_slice(data))
            .is_some()
    }

    fn keep(&mut self, block: u64, verified: &[u8; BLOCK]) {
        self.verified.push((block, *verified));
    }
}

fn data(n: u64) -> Vec<u8> {
    let mut bytes = vec![0; n as usize * BLOCK];
    for (i, b) in bytes.iter_mut().enumerate() {
        if (i / BLOCK) % 3 != 1 {
            *b = (i * 7 + i / BLOCK) as u8;
        }
    }
    bytes
}

fn built(n: u64) -> (Geometry, Image, Digest) {
    let geometry = Geometry::new(n).unwrap();
    let mut bytes = data(n);
    let mut tree = vec![0; geometry.tree_blocks() as usize * BLOCK];
    let mut builder = Builder::new(geometry, &mut tree).unwrap();
    for block in bytes.chunks_exact(BLOCK) {
        builder.push(block.try_into().unwrap());
    }
    let root = builder.finish().unwrap();
    bytes.extend_from_slice(&tree);
    let image = Image {
        blocks: bytes,
        verified: Vec::new(),
        reads: 0,
        lookups: 0,
    };
    (geometry, image, root)
}

fn read_back(
    geometry: &Geometry,
    image: &mut Image,
    root: &Digest,
    index: u64,
) -> Result<[u8; BLOCK], Fault<()>> {
    let (mut out, mut scratch) = ([0; BLOCK], [0; BLOCK]);
    read(geometry, root, image, index, &mut out, &mut scratch).map(|()| out)
}

#[test]
fn every_block_reads_back_verified_and_a_tree_block_once() {
    for n in [1, 2, 128, 129, 300] {
        let (geometry, mut image, root) = built(n);
        let bytes = data(n);
        for index in 0..n {
            let at = index as usize * BLOCK;
            assert_eq!(
                read_back(&geometry, &mut image, &root, index).map(|b| b.to_vec()),
                Ok(bytes[at..at + BLOCK].to_vec()),
                "{n} {index}"
            );
        }
        assert_eq!(
            image.reads as u64,
            n + geometry.tree_blocks(),
            "each tree block is read once"
        );
        assert_eq!(
            read_back(&geometry, &mut image, &root, n),
            Err(Fault::OutOfRange)
        );
    }
}

#[test]
fn a_flipped_bit_fails_the_read_that_meets_it() {
    let (geometry, mut image, root) = built(300);
    image.blocks[200 * BLOCK + 17] ^= 0x10;
    assert_eq!(
        read_back(&geometry, &mut image, &root, 200),
        Err(Fault::Mismatch(200))
    );
    assert!(read_back(&geometry, &mut image, &root, 199).is_ok());

    let (geometry, mut image, root) = built(300);
    let (leaf, _) = geometry.digest_at(0, 299).unwrap();
    image.blocks[leaf as usize * BLOCK + 5] ^= 1;
    assert_eq!(
        read_back(&geometry, &mut image, &root, 299),
        Err(Fault::Mismatch(leaf))
    );
    assert!(read_back(&geometry, &mut image, &root, 0).is_ok());

    let (geometry, mut image, mut root) = built(2);
    root[0] ^= 1;
    assert_eq!(
        read_back(&geometry, &mut image, &root, 0),
        Err(Fault::Mismatch(geometry.top()))
    );
    let (geometry, mut image, mut root) = built(1);
    root[31] ^= 1;
    assert_eq!(
        read_back(&geometry, &mut image, &root, 0),
        Err(Fault::Mismatch(0))
    );
}

#[test]
fn a_run_checks_out_reading_each_leaf_once() {
    for (n, first, count) in [
        (1, 0, 1),
        (300, 0, 300),
        (300, 100, 60),
        (16385, 16000, 385),
    ] {
        let (geometry, mut image, root) = built(n);
        let bytes = data(n);
        let run = &bytes[first as usize * BLOCK..(first + count) as usize * BLOCK];
        let mut scratch = [0; BLOCK];
        assert_eq!(
            check(&geometry, &root, &mut image, first, run, &mut scratch),
            Ok(()),
            "{n} {first}"
        );
        let leaves = match geometry.level(0) {
            Some(_) => (first + count - 1) / 128 - first / 128 + 1,
            None => 0,
        };
        let levels = geometry.levels() as u64;
        assert!(
            image.lookups as u64 <= leaves * levels,
            "{n} {first}: {} lookups",
            image.lookups
        );
        assert!(image.reads as u64 <= leaves * levels, "{n} {first}");
    }
}

#[test]
fn a_run_hashed_already_checks_out_as_its_bytes_do() {
    for (n, first, count) in [(1, 0, 1), (300, 100, 60), (16385, 16000, 385)] {
        let (geometry, mut image, root) = built(n);
        let bytes = data(n);
        let run = &bytes[first as usize * BLOCK..(first + count) as usize * BLOCK];
        let mut digests: Vec<Digest> = run
            .chunks_exact(BLOCK)
            .map(|block| hash(block.try_into().unwrap()))
            .collect();
        let mut scratch = [0; BLOCK];
        assert_eq!(
            check_digests(&geometry, &root, &mut image, first, &digests, &mut scratch),
            Ok(()),
            "{n} {first}"
        );
        let last = digests.len() - 1;
        digests[last][3] ^= 1;
        assert_eq!(
            check_digests(&geometry, &root, &mut image, first, &digests, &mut scratch),
            Err(Fault::Mismatch(first + count - 1)),
            "{n} {first}"
        );
        if count > 1 {
            assert_eq!(
                check_digests(&geometry, &root, &mut image, n - 1, &digests, &mut scratch),
                Err(Fault::OutOfRange)
            );
        }
    }
}

#[test]
fn a_run_fails_at_the_block_its_tree_refuses() {
    let (geometry, mut image, root) = built(300);
    let mut run = data(300)[..200 * BLOCK].to_vec();
    run[150 * BLOCK + 9] ^= 0x80;
    let mut scratch = [0; BLOCK];
    assert_eq!(
        check(&geometry, &root, &mut image, 0, &run, &mut scratch),
        Err(Fault::Mismatch(150))
    );
    assert_eq!(
        check(
            &geometry,
            &root,
            &mut image,
            250,
            &run[..51 * BLOCK],
            &mut scratch
        ),
        Err(Fault::OutOfRange)
    );
    assert_eq!(
        check(
            &geometry,
            &root,
            &mut image,
            0,
            &run[..BLOCK + 1],
            &mut scratch
        ),
        Err(Fault::OutOfRange)
    );
}

#[test]
fn a_tree_block_reads_back_verified() {
    let (geometry, mut image, root) = built(16385);
    let mut out = [0; BLOCK];
    for level in 0..geometry.levels() {
        let (start, count) = geometry.level(level).unwrap();
        for index in [0, count - 1] {
            assert_eq!(
                tree_block(&geometry, &root, &mut image, level, index, &mut out),
                Ok(())
            );
            let at = (start + index) as usize * BLOCK;
            assert_eq!(out[..], image.blocks[at..at + BLOCK]);
        }
        assert_eq!(
            tree_block(&geometry, &root, &mut image, level, count, &mut out),
            Err(Fault::OutOfRange)
        );
    }
    let (start, _) = geometry.level(1).unwrap();
    let (geometry, mut image, root) = built(16385);
    image.blocks[start as usize * BLOCK + 3] ^= 1;
    assert_eq!(
        tree_block(&geometry, &root, &mut image, 0, 0, &mut out),
        Err(Fault::Mismatch(start))
    );
}

#[test]
fn a_tree_checks_out_to_its_root_and_any_flipped_bit_fails_it() {
    for n in [1, 2, 128, 129, 300, 16385] {
        let (geometry, image, root) = built(n);
        let from = geometry.top() as usize * BLOCK;
        let tree = &image.blocks[from..];
        let compress = sha256::compress_blocks;
        assert_eq!(
            verify_tree(&geometry, &root, compress, tree, 0),
            Ok(()),
            "{n}"
        );
        for lowest in 1..geometry.levels() {
            let (start, count) = geometry.level(lowest).unwrap();
            let upper = &image.blocks[from..(start + count) as usize * BLOCK];
            assert_eq!(
                verify_tree(&geometry, &root, compress, upper, lowest),
                Ok(()),
                "{n} {lowest}"
            );
            assert_eq!(
                verify_tree(&geometry, &root, compress, tree, lowest),
                Err(Fault::OutOfRange)
            );
        }
        assert_eq!(
            verify_tree(&geometry, &root, compress, &tree[BLOCK..], 0),
            Err(Fault::OutOfRange)
        );
        let flips = if n < 1000 {
            vec![geometry.top(), geometry.image_blocks() - 1]
        } else {
            vec![
                geometry.top(),
                geometry.top() + 1,
                geometry.image_blocks() - 1,
            ]
        };
        for block in flips {
            let mut bent = tree.to_vec();
            bent[(block - geometry.top()) as usize * BLOCK + 9] ^= 2;
            assert!(
                matches!(
                    verify_tree(&geometry, &root, compress, &bent, 0),
                    Err(Fault::Mismatch(at)) if at == block
                ),
                "{n} {block}"
            );
        }
    }
}

#[test]
fn a_hash_takes_whatever_compresses_its_blocks() {
    fn none(_: &mut [u32; 8], _: &[u8]) {}
    let block = [7; BLOCK];
    assert_eq!(hash_using(sha256::compress_blocks, &block), hash(&block));
    assert_ne!(hash_using(none, &block), hash(&block));
    assert_eq!(hash_using(none, &[0; BLOCK]), hash(&[0; BLOCK]));
    let mut split = sha256::Sha256::using(sha256::compress_blocks);
    for piece in [&[1u8; 3][..], &[2; 64], &[3; 129], &[4; 61]] {
        split.update(piece);
    }
    let whole: Vec<u8> = [&[1u8; 3][..], &[2; 64], &[3; 129], &[4; 61]].concat();
    assert_eq!(split.finish(), sha256::digest(&whole));
}

#[test]
fn a_builder_takes_exactly_its_data() {
    let geometry = Geometry::new(3).unwrap();
    let mut tree = vec![0; BLOCK];
    assert!(Builder::new(geometry, &mut []).is_none());
    let mut builder = Builder::new(geometry, &mut tree).unwrap();
    builder.push(&[1; BLOCK]);
    assert!(builder.finish().is_none());
}

/// The tree and root `veritysetup` writes for the same data, where it is
/// installed.
#[test]
fn veritysetup_builds_the_same_tree() {
    use std::process::Command;
    if Command::new("veritysetup")
        .arg("--version")
        .output()
        .is_err()
    {
        std::eprintln!("veritysetup is not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(std::format!("slopos-verity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for n in [1u64, 2, 129, 16385] {
        let (geometry, image, root) = built(n);
        let file = dir.join(std::format!("{n}.img"));
        let data_len = n as usize * BLOCK;
        std::fs::write(&file, &image.blocks[..data_len]).unwrap();
        let root_file = dir.join(std::format!("{n}.root"));
        let status = Command::new("veritysetup")
            .arg("format")
            .args([&file, &file])
            .args([
                "--format=1",
                "--hash=sha256",
                "--data-block-size=4096",
                "--hash-block-size=4096",
                "--salt=-",
                "--no-superblock",
            ])
            .arg(std::format!("--data-blocks={n}"))
            .arg(std::format!("--hash-offset={data_len}"))
            .arg("--root-hash-file")
            .arg(&root_file)
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        let theirs = std::fs::read_to_string(&root_file).unwrap();
        assert_eq!(theirs.trim(), hex(&root), "root of {n} blocks");
        let written = std::fs::read(&file).unwrap();
        assert_eq!(written.len() as u64, geometry.image_bytes(), "{n}");
        assert!(written == image.blocks, "tree of {n} blocks");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
