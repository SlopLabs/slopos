//! e2fsprogs as the oracle: images `mke2fs` and `debugfs` wrote are read and
//! checked with this crate's codecs, and what this crate writes is handed to
//! `e2fsck`. Needs `mke2fs`, `debugfs` and `e2fsck` on PATH.

use slopos_ext4_core::bytes::{le16, le32, put_le16, put_le32};
use slopos_ext4_core::extent::{self, Extent, ExtentError, Node, Store};
use slopos_ext4_core::group::{self, Desc, DescCsum};
use slopos_ext4_core::inode::{self, off as ioff};
use slopos_ext4_core::jbd2::{self, Csum, Tag, tag_flag};
use slopos_ext4_core::recovery::{self, JournalIo, RevokeTable};
use slopos_ext4_core::superblock::{self as sbk, incompat, off as soff};
use slopos_ext4_core::{dir, profile};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ext4-core-interop-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{cmd:?}: {e} (e2fsprogs must be on PATH)"));
    out
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A profile volume of `mib` MiB, made the way the image builder makes one.
fn mkfs(path: &Path, mib: u64, extra: &[&str]) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(mib << 20).unwrap();
    let mut cmd = Command::new("mke2fs");
    cmd.args([
        "-F",
        "-q",
        "-t",
        "ext4",
        "-O",
        "none",
        "-O",
        profile::features(),
    ])
    .args(["-I", &profile::inode_size().to_string()])
    .args(["-b", &profile::block_size().to_string()])
    .args(extra)
    .arg(path);
    let out = run(&mut cmd);
    assert!(out.status.success(), "mke2fs: {}", text(&out));
}

fn debugfs(path: &Path, script: &str) -> String {
    let script_path = path.with_extension("cmds");
    std::fs::write(&script_path, script).unwrap();
    let out = run(Command::new("debugfs")
        .arg("-w")
        .arg("-f")
        .arg(&script_path)
        .arg(path));
    text(&out)
}

fn debugfs_ro(path: &Path, request: &str) -> String {
    text(&run(Command::new("debugfs")
        .arg("-R")
        .arg(request)
        .arg(path)))
}

/// `e2fsck -f` with `-n` or `-y`; the exit code and the transcript.
fn e2fsck(path: &Path, flag: &str) -> (i32, String) {
    let out = run(Command::new("e2fsck").arg("-f").arg(flag).arg(path));
    (out.status.code().unwrap_or(-1), text(&out))
}

struct Image {
    bytes: Vec<u8>,
    bs: usize,
}

impl Image {
    fn load(path: &Path) -> Self {
        let bytes = std::fs::read(path).unwrap();
        let bs = 1024usize << le32(&bytes[1024..2048], soff::LOG_BLOCK_SIZE);
        Self { bytes, bs }
    }

    fn save(&self, path: &Path) {
        std::fs::write(path, &self.bytes).unwrap();
    }

    fn sb(&self) -> &[u8] {
        &self.bytes[1024..2048]
    }

    fn sb_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[1024..2048]
    }

    fn block(&self, n: u64) -> &[u8] {
        let at = n as usize * self.bs;
        &self.bytes[at..at + self.bs]
    }

    fn block_mut(&mut self, n: u64) -> &mut [u8] {
        let at = n as usize * self.bs;
        &mut self.bytes[at..at + self.bs]
    }

    fn seed(&self) -> u32 {
        sbk::csum_seed(self.sb())
    }

    fn desc_size(&self) -> usize {
        usize::from(sbk::desc_size(self.sb()))
    }

    fn groups(&self) -> u32 {
        let first = u64::from(le32(self.sb(), soff::FIRST_DATA_BLOCK));
        let bpg = u64::from(le32(self.sb(), soff::BLOCKS_PER_GROUP));
        ((sbk::blocks_count(self.sb()) - first).div_ceil(bpg)) as u32
    }

    fn desc_range(&self, g: u32) -> std::ops::Range<usize> {
        let first = le32(self.sb(), soff::FIRST_DATA_BLOCK) as usize;
        let at = (first + 1) * self.bs + g as usize * self.desc_size();
        at..at + self.desc_size()
    }

    fn desc(&self, g: u32) -> Desc {
        Desc::parse(&self.bytes[self.desc_range(g)])
    }

    fn inode_range(&self, ino: u32) -> std::ops::Range<usize> {
        let ipg = le32(self.sb(), soff::INODES_PER_GROUP);
        let isz = usize::from(le16(self.sb(), soff::INODE_SIZE));
        let d = self.desc((ino - 1) / ipg);
        let at = d.inode_table as usize * self.bs + ((ino - 1) % ipg) as usize * isz;
        at..at + isz
    }

    fn inode(&self, ino: u32) -> &[u8] {
        &self.bytes[self.inode_range(ino)]
    }

    fn inode_seed(&self, ino: u32) -> u32 {
        inode::seed(self.seed(), ino, le32(self.inode(ino), ioff::GENERATION))
    }
}

/// An inode's extent tree inside a loaded image. Nodes are allocated from a
/// pool the test reserved; every write seals the tail.
struct ImageTree<'a> {
    img: &'a mut Image,
    ino: u32,
    pool: Vec<u64>,
    freed_nodes: Vec<u64>,
}

impl Store for ImageTree<'_> {
    type Error = ExtentError;

    fn block_size(&self) -> usize {
        self.img.bs
    }

    fn read<R>(&mut self, node: Node, f: impl FnOnce(&[u8]) -> R) -> Result<R, ExtentError> {
        match node {
            Node::Root => {
                let r = self.img.inode_range(self.ino);
                Ok(f(
                    &self.img.bytes[r.start + ioff::BLOCK..r.start + ioff::BLOCK + 60]
                ))
            }
            Node::Block(b) => {
                let seed = self.img.inode_seed(self.ino);
                assert!(extent::verify(seed, self.img.block(b)), "tail of {b}");
                Ok(f(self.img.block(b)))
            }
        }
    }

    fn write<R>(&mut self, node: Node, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, ExtentError> {
        let seed = self.img.inode_seed(self.ino);
        match node {
            Node::Root => {
                let r = self.img.inode_range(self.ino);
                Ok(f(
                    &mut self.img.bytes[r.start + ioff::BLOCK..r.start + ioff::BLOCK + 60]
                ))
            }
            Node::Block(b) => {
                let block = self.img.block_mut(b);
                let out = f(block);
                extent::seal(seed, block);
                Ok(out)
            }
        }
    }

    fn alloc_node(&mut self, _goal: u64) -> Result<u64, ExtentError> {
        let b = self.pool.pop().ok_or(ExtentError::TooDeep)?;
        self.img.block_mut(b).fill(0);
        Ok(b)
    }

    fn free_node(&mut self, block: u64) -> Result<(), ExtentError> {
        self.freed_nodes.push(block);
        Ok(())
    }

    fn free_data(&mut self, _first: u64, _count: u32) -> Result<(), ExtentError> {
        Ok(())
    }
}

/// The free blocks of the last group, from its bitmap: room a test may take
/// without colliding with what mke2fs placed.
fn free_tail_blocks(img: &Image, want: usize) -> Vec<u64> {
    let g = img.groups() - 1;
    let d = img.desc(g);
    assert_eq!(d.flags & group::BLOCK_UNINIT, 0);
    let bpg = u64::from(le32(img.sb(), soff::BLOCKS_PER_GROUP));
    let base = u64::from(g) * bpg + u64::from(le32(img.sb(), soff::FIRST_DATA_BLOCK));
    let bitmap = img.block(d.block_bitmap).to_vec();
    let total = sbk::blocks_count(img.sb());
    let mut out = Vec::new();
    for bit in 0..bpg {
        let b = base + bit;
        if b >= total {
            break;
        }
        if bitmap[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
            out.push(b);
            if out.len() == want {
                break;
            }
        }
    }
    assert_eq!(out.len(), want, "last group has room");
    out
}

/// Extents as `debugfs ex` lists the leaves.
fn debugfs_extents(path: &Path, file: &str) -> Vec<Extent> {
    let out = debugfs_ro(path, &format!("ex {file}"));
    let mut v = Vec::new();
    for line in out.lines().skip(1) {
        let cols: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '/' || c == '-')
            .filter(|s| !s.is_empty())
            .collect();
        if cols.len() < 8 || cols[0] != cols[1] {
            continue;
        }
        let lblk: u32 = cols[4].parse().unwrap();
        let pblk: u64 = cols[6].parse().unwrap();
        let len: u32 = cols[8].parse().unwrap();
        v.push(Extent {
            lblk,
            len,
            pblk,
            unwritten: line.contains("Uninit"),
        });
    }
    v
}

fn inode_of(path: &Path, file: &str) -> u32 {
    let out = debugfs_ro(path, &format!("stat {file}"));
    let at = out.find("Inode: ").expect("stat") + 7;
    out[at..]
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn profile_image_checksums_verify() {
    let s = Scratch::new();
    let path = s.path("p.img");
    mkfs(&path, 128, &[]);
    let data = s.path("data");
    std::fs::write(&data, vec![0x5Au8; 3 * 4096 + 17]).unwrap();
    debugfs(
        &path,
        &format!(
            "write {} file\nmkdir d\nsymlink l /some/target\n",
            data.display()
        ),
    );
    let img = Image::load(&path);
    let sb = img.sb();
    assert!(sbk::verify(sb), "superblock checksum");
    assert_eq!(sb[soff::CHECKSUM_TYPE], sbk::CHECKSUM_CRC32C);
    let (c, i, r) = profile::feature_words().unwrap();
    assert_eq!(le32(sb, soff::FEATURE_COMPAT), c);
    assert_eq!(le32(sb, soff::FEATURE_INCOMPAT), i);
    assert_eq!(le32(sb, soff::FEATURE_RO_COMPAT), r);

    let seed = img.seed();
    let kind = DescCsum::Crc32c { seed };
    let bpg = le32(sb, soff::BLOCKS_PER_GROUP);
    let ipg = le32(sb, soff::INODES_PER_GROUP);
    for g in 0..img.groups() {
        let raw = &img.bytes[img.desc_range(g)];
        assert!(group::verify(kind, g, raw), "descriptor {g}");
        let d = Desc::parse(raw);
        if d.flags & group::BLOCK_UNINIT == 0 {
            let sum = group::bitmap_checksum(seed, img.block(d.block_bitmap), bpg);
            assert_eq!(
                d.block_bitmap_csum,
                group::stored_bitmap_csum(sum, img.desc_size()),
                "block bitmap {g}"
            );
        }
        if d.flags & group::INODE_UNINIT == 0 {
            let sum = group::bitmap_checksum(seed, img.block(d.inode_bitmap), ipg);
            assert_eq!(
                d.inode_bitmap_csum,
                group::stored_bitmap_csum(sum, img.desc_size()),
                "inode bitmap {g}"
            );
        }
    }
    let file = inode_of(&path, "/file");
    for ino in [
        2,
        7,
        8,
        11,
        file,
        inode_of(&path, "/d"),
        inode_of(&path, "/l"),
    ] {
        assert!(inode::verify(seed, ino, img.inode(ino)), "inode {ino}");
    }
    let root_block = {
        let mut tree = ImageTree {
            img: &mut Image::load(&path),
            ino: 2,
            pool: vec![],
            freed_nodes: vec![],
        };
        extent::lookup(&mut tree, 0).unwrap().unwrap().pblk
    };
    assert!(
        dir::verify(img.inode_seed(2), img.block(root_block)),
        "root directory tail"
    );
    let jsb = jbd2::Superblock::parse(img.block({
        let mut tree = ImageTree {
            img: &mut Image::load(&path),
            ino: 8,
            pool: vec![],
            freed_nodes: vec![],
        };
        extent::lookup(&mut tree, 0).unwrap().unwrap().pblk
    }))
    .unwrap();
    assert_eq!((jsb.first, jsb.start, jsb.sequence), (1, 0, 1));
    assert_eq!(jsb.block_size as usize, img.bs);
}

#[test]
fn gdt_csum_descriptors_verify() {
    let s = Scratch::new();
    let path = s.path("g.img");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(64 << 20).unwrap();
    let out = run(Command::new("mke2fs")
        .args([
            "-F",
            "-q",
            "-t",
            "ext4",
            "-O",
            "^metadata_csum,uninit_bg,^64bit",
            "-b",
            "1024",
        ])
        .arg(&path));
    assert!(out.status.success(), "{}", text(&out));
    let img = Image::load(&path);
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&img.sb()[soff::UUID..soff::UUID + 16]);
    for g in 0..img.groups() {
        assert!(
            group::verify(DescCsum::Crc16 { uuid }, g, &img.bytes[img.desc_range(g)]),
            "descriptor {g}"
        );
    }
}

#[test]
fn extent_lookup_matches_debugfs() {
    let s = Scratch::new();
    let path = s.path("x.img");
    mkfs(&path, 64, &[]);
    let data = s.path("data");
    std::fs::write(&data, vec![1u8; 4096 * 600]).unwrap();
    let mut script = format!("write {} f\nfallocate /f 600 700\n", data.display());
    for k in 0..120u32 {
        script += &format!("punch /f {} {}\n", 5 * k, 5 * k + 1);
    }
    debugfs(&path, &script);
    let theirs = debugfs_extents(&path, "/f");
    assert!(theirs.len() > 100, "a tree with depth: {}", theirs.len());
    let ino = inode_of(&path, "/f");
    let mut img = Image::load(&path);
    let mut tree = ImageTree {
        img: &mut img,
        ino,
        pool: vec![],
        freed_nodes: vec![],
    };
    let mut ours = Vec::new();
    extent::for_each(&mut tree, &mut |e| {
        ours.push(e);
        true
    })
    .unwrap();
    assert_eq!(ours, theirs);
    for e in &theirs {
        let m = extent::lookup(&mut tree, e.lblk + e.len - 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            (m.pblk, m.len, m.unwritten),
            (e.pblk + u64::from(e.len) - 1, 1, e.unwritten)
        );
    }
    assert_eq!(extent::lookup(&mut tree, 0).unwrap(), None);
}

/// Grow a tree with this crate's insert and hand the volume to e2fsck, which
/// may fix only the allocation bookkeeping the test leaves to it.
fn build_tree_and_check(pattern: &dyn Fn(u32) -> bool, extents: u32, truncate_to: Option<u32>) {
    let s = Scratch::new();
    let path = s.path("t.img");
    mkfs(&path, 256, &[]);
    let one = s.path("one");
    std::fs::write(&one, [7u8; 10]).unwrap();
    debugfs(&path, &format!("write {} f\n", one.display()));
    let ino = inode_of(&path, "/f");
    let mut img = Image::load(&path);
    let bs = img.bs;
    let pool_size = 200usize;
    let mut free = free_tail_blocks(&img, 2 * extents as usize + pool_size);
    let pool = free.split_off(free.len() - pool_size);
    {
        let r = img.inode_range(ino);
        let rec = &mut img.bytes[r];
        extent::init_root(&mut rec[ioff::BLOCK..ioff::BLOCK + 60]);
    }
    let mut tree = ImageTree {
        img: &mut img,
        ino,
        pool,
        freed_nodes: vec![],
    };
    let mut mapped = Vec::new();
    let mut lblk = 0u32;
    let mut next = free.into_iter();
    while mapped.len() < extents as usize {
        if pattern(lblk) {
            let pblk = next.next().unwrap();
            extent::insert(
                &mut tree,
                Extent {
                    lblk,
                    len: 1,
                    pblk,
                    unwritten: false,
                },
                pblk,
            )
            .unwrap();
            mapped.push((lblk, pblk));
        }
        lblk += 1;
    }
    if let Some(to) = truncate_to {
        extent::truncate(&mut tree, to).unwrap();
        mapped.retain(|&(l, _)| l < to);
    }
    let mut nodes = Vec::new();
    extent::for_each_node(&mut tree, &mut |b| {
        nodes.push(b);
        true
    })
    .unwrap();
    let size = mapped.last().map_or(0, |&(l, _)| u64::from(l) + 1) * bs as u64;
    let blocks = (mapped.len() + nodes.len()) as u64 * (bs as u64 / 512);
    let seed = tree.img.seed();
    let r = tree.img.inode_range(ino);
    let rec = &mut tree.img.bytes[r];
    put_le32(rec, ioff::SIZE_LO, size as u32);
    put_le32(rec, ioff::SIZE_HIGH, (size >> 32) as u32);
    put_le32(rec, ioff::BLOCKS_LO, blocks as u32);
    put_le16(rec, ioff::BLOCKS_HIGH, (blocks >> 32) as u16);
    inode::seal(seed, ino, rec);
    img.save(&path);

    let (_, fixed) = e2fsck(&path, "-y");
    for line in fixed.lines() {
        let allowed = line.contains("bitmap differences")
            || line.contains("count wrong")
            || line.starts_with("Fix")
            || line.starts_with("Pass ")
            || line.starts_with("e2fsck ")
            || line.starts_with(' ')
            || line.trim().is_empty()
            || line.contains("MODIFIED")
            || line.contains(" files (")
            || line.contains("Padding at end");
        assert!(allowed, "e2fsck objected to the tree: {line}\n{fixed}");
    }
    let (code, clean) = e2fsck(&path, "-n");
    assert_eq!(code, 0, "{clean}");
    let theirs: BTreeMap<u32, u64> = debugfs_extents(&path, "/f")
        .into_iter()
        .flat_map(|e| (0..e.len).map(move |k| (e.lblk + k, e.pblk + u64::from(k))))
        .collect();
    assert_eq!(theirs, mapped.into_iter().collect::<BTreeMap<_, _>>());
}

#[test]
fn sequential_tree_passes_e2fsck() {
    build_tree_and_check(&|_| true, 3000, None);
}

#[test]
fn fragmented_tree_passes_e2fsck() {
    build_tree_and_check(&|l| l % 2 == 0, 3000, None);
}

#[test]
fn truncated_tree_passes_e2fsck() {
    build_tree_and_check(&|l| l % 3 != 0, 3000, Some(1500));
    build_tree_and_check(&|l| l % 2 == 0, 3000, Some(5));
}

struct FileJournal<'a> {
    img: &'a mut Image,
    map: Vec<u64>,
    homes: BTreeSet<u64>,
}

impl JournalIo for FileJournal<'_> {
    type Error = ();

    fn read(&mut self, block: u32, buf: &mut [u8]) -> Result<(), ()> {
        buf.copy_from_slice(self.img.block(self.map[block as usize]));
        Ok(())
    }

    fn write_home(&mut self, block: u64, data: &[u8]) -> Result<(), ()> {
        self.homes.insert(block);
        self.img.block_mut(block).copy_from_slice(data);
        Ok(())
    }
}

#[derive(Default)]
struct Revokes(BTreeMap<u64, u32>);

impl RevokeTable for Revokes {
    fn note(&mut self, block: u64, age: u32) -> bool {
        let newest = self.0.entry(block).or_insert(age);
        *newest = (*newest).max(age);
        true
    }

    fn newest(&mut self, block: u64) -> Option<u32> {
        self.0.get(&block).copied()
    }
}

fn journal_map(img: &mut Image) -> Vec<u64> {
    let mut tree = ImageTree {
        img,
        ino: 8,
        pool: vec![],
        freed_nodes: vec![],
    };
    let mut map = Vec::new();
    extent::for_each(&mut tree, &mut |e| {
        assert_eq!(e.lblk as usize, map.len(), "the journal has no holes");
        map.extend((0..u64::from(e.len)).map(|k| e.pblk + k));
        true
    })
    .unwrap();
    map
}

/// Our replay of a journal debugfs wrote, against e2fsck's.
#[test]
fn replay_matches_e2fsck() {
    let s = Scratch::new();
    let base = s.path("base.img");
    mkfs(&base, 64, &[]);
    let img = Image::load(&base);
    let targets = free_tail_blocks(&img, 6);
    let [a, b, c, d, e, f] = [
        targets[0], targets[1], targets[2], targets[3], targets[4], targets[5],
    ];
    let mut payload = vec![0u8; 4096 * 3];
    for (i, chunk) in payload.chunks_mut(4096).enumerate() {
        chunk.fill(0x40 + i as u8);
    }
    payload[4096..4100].copy_from_slice(&jbd2::MAGIC.to_be_bytes());
    let src = s.path("payload");
    std::fs::write(&src, &payload).unwrap();
    let p = src.display();
    // T1 logs a, b (escaped) and c; T2 revokes a; T3 logs a and d. debugfs
    // commits no `jw` given both blocks and revokes, so e and f never replay.
    let out = debugfs(
        &base,
        &format!(
            "jo -c\njw -b {a},{b},{c} {p}\njw -r {a} /dev/null\njw -b {a},{d} {p}\njw -b {e} -r {e} {p}\njw -b {f} -c {p}\njc\n"
        ),
    );
    assert!(!out.contains("rror"), "{out}");

    let theirs = s.path("theirs.img");
    std::fs::copy(&base, &theirs).unwrap();
    let (code, log) = e2fsck(&theirs, "-y");
    assert!(code == 0 || code == 1, "{log}");
    let theirs = Image::load(&theirs);

    let mut ours = Image::load(&base);
    let map = journal_map(&mut ours);
    let jsb = jbd2::Superblock::parse(ours.block(map[0])).unwrap();
    assert_eq!(jsb.csum(), Csum::V3);
    let total = sbk::blocks_count(ours.sb());
    let bs = ours.bs;
    let (mut meta, mut data) = (vec![0u8; bs], vec![0u8; bs]);
    let mut io = FileJournal {
        img: &mut ours,
        map,
        homes: BTreeSet::new(),
    };
    let mut revokes = Revokes::default();
    let done = recovery::recover(&mut io, &mut revokes, &jsb, &mut meta, &mut data, &|b| {
        b < total
    })
    .unwrap();
    assert_eq!(done.transactions, 3);
    assert_eq!(io.homes, [a, b, c, d].into_iter().collect());
    for blk in [a, b, c, d, e, f] {
        assert_eq!(ours.block(blk), theirs.block(blk), "block {blk}");
    }
    assert_eq!(
        &ours.block(b)[..4],
        &jbd2::MAGIC.to_be_bytes(),
        "escape undone"
    );
    assert_eq!(
        ours.block(a)[100],
        0x40,
        "T3's copy of a, after T2's revoke"
    );
}

/// A transaction larger than one descriptor holds: debugfs fills each
/// descriptor to its room and marks no tag last, as Linux does.
#[test]
fn replay_spans_full_descriptors_like_e2fsck() {
    let s = Scratch::new();
    let base = s.path("big.img");
    mkfs(&base, 64, &[]);
    let img = Image::load(&base);
    let targets = free_tail_blocks(&img, 300);
    let mut payload = vec![0u8; 4096 * targets.len()];
    for (i, chunk) in payload.chunks_mut(4096).enumerate() {
        chunk.fill(i as u8 | 1);
    }
    let src = s.path("payload");
    std::fs::write(&src, &payload).unwrap();
    let list: Vec<String> = targets.iter().map(u64::to_string).collect();
    let out = debugfs(
        &base,
        &format!("jo -c\njw -b {} {}\njc\n", list.join(","), src.display()),
    );
    assert!(!out.contains("rror"), "{out}");

    let theirs = s.path("theirs.img");
    std::fs::copy(&base, &theirs).unwrap();
    let (code, log) = e2fsck(&theirs, "-y");
    assert!(code == 0 || code == 1, "{log}");
    let theirs = Image::load(&theirs);

    let mut ours = Image::load(&base);
    let map = journal_map(&mut ours);
    let jsb = jbd2::Superblock::parse(ours.block(map[0])).unwrap();
    let total = sbk::blocks_count(ours.sb());
    let bs = ours.bs;
    let (mut meta, mut data) = (vec![0u8; bs], vec![0u8; bs]);
    let mut io = FileJournal {
        img: &mut ours,
        map,
        homes: BTreeSet::new(),
    };
    let done = recovery::recover(
        &mut io,
        &mut Revokes::default(),
        &jsb,
        &mut meta,
        &mut data,
        &|b| b < total,
    )
    .unwrap();
    assert_eq!((done.transactions, done.blocks), (1, 300));
    for (i, &t) in targets.iter().enumerate() {
        assert_eq!(ours.block(t), theirs.block(t), "block {t}");
        assert_eq!(ours.block(t)[0], i as u8 | 1);
    }
}

/// A journal this crate wrote, replayed by e2fsck: T1 logs four blocks and
/// revokes the last two, and T2 logs the third again.
#[test]
fn e2fsck_replays_our_journal() {
    let s = Scratch::new();
    let path = s.path("w.img");
    mkfs(&path, 64, &[]);
    let mut img = Image::load(&path);
    let targets = free_tail_blocks(&img, 5);
    let map = journal_map(&mut img);
    let bs = img.bs;
    let jsb_block = map[0];
    let mut jsb_raw = img.block(jsb_block).to_vec();
    let incompat_bits = jbd2::feature::INCOMPAT_64BIT
        | jbd2::feature::INCOMPAT_CSUM_V3
        | jbd2::feature::INCOMPAT_REVOKE;
    jbd2::set_features(&mut jsb_raw, incompat_bits);
    let jsb = jbd2::Superblock::parse(&jsb_raw).unwrap();
    let fmt = jsb.format();
    let seq = 0x1234u32;

    let mut copies: Vec<Vec<u8>> = (0..4).map(|i| vec![0x10 + i as u8; bs]).collect();
    copies[1][..4].copy_from_slice(&jbd2::MAGIC.to_be_bytes());
    let mut at = 1usize;
    let mut descriptor = vec![0u8; bs];
    jbd2::begin_descriptor(&mut descriptor, seq);
    for (i, copy) in copies.iter().enumerate() {
        let mut stored = copy.clone();
        let mut flags = 0;
        if jbd2::needs_escape(&stored) {
            jbd2::escape(&mut stored);
            flags |= tag_flag::ESCAPE;
        }
        let checksum = fmt.data_checksum(seq, &stored);
        jbd2::put_tag(
            &fmt,
            &mut descriptor,
            i,
            copies.len(),
            Tag {
                block: targets[i],
                flags,
                checksum,
            },
        );
        img.block_mut(map[at + 1 + i]).copy_from_slice(&stored);
    }
    jbd2::seal_descriptor(&fmt, &mut descriptor);
    img.block_mut(map[at]).copy_from_slice(&descriptor);
    at += 1 + copies.len();
    let mut revoke = vec![0u8; bs];
    jbd2::encode_revoke(&fmt, &mut revoke, seq, &[targets[2], targets[3]]);
    img.block_mut(map[at]).copy_from_slice(&revoke);
    at += 1;
    let mut commit = vec![0u8; bs];
    jbd2::encode_commit(&fmt, &mut commit, seq, 1_700_000_000, 7);
    img.block_mut(map[at]).copy_from_slice(&commit);

    let mut descriptor = vec![0u8; bs];
    jbd2::begin_descriptor(&mut descriptor, seq + 1);
    let late = vec![0x77u8; bs];
    jbd2::put_tag(
        &fmt,
        &mut descriptor,
        0,
        1,
        Tag {
            block: targets[2],
            flags: 0,
            checksum: fmt.data_checksum(seq + 1, &late),
        },
    );
    jbd2::seal_descriptor(&fmt, &mut descriptor);
    img.block_mut(map[at + 1]).copy_from_slice(&descriptor);
    img.block_mut(map[at + 2]).copy_from_slice(&late);
    jbd2::encode_commit(&fmt, &mut commit, seq + 1, 1_700_000_001, 0);
    img.block_mut(map[at + 3]).copy_from_slice(&commit);

    jbd2::set_log_state(&mut jsb_raw, seq, 1);
    img.block_mut(jsb_block).copy_from_slice(&jsb_raw);
    let inc = le32(img.sb(), soff::FEATURE_INCOMPAT) | incompat::RECOVER;
    put_le32(img.sb_mut(), soff::FEATURE_INCOMPAT, inc);
    sbk::seal(img.sb_mut());
    img.save(&path);
    let ours_path = s.path("ours.img");
    img.save(&ours_path);

    let logdump = debugfs_ro(&path, "logdump");
    assert!(
        logdump.contains("Found expected sequence 4661, type 2 (commit block)"),
        "{logdump}"
    );
    let (code, log) = e2fsck(&path, "-y");
    assert!(code == 0 || code == 1, "{log}");
    assert!(log.contains("recovering journal"), "{log}");
    assert!(!log.contains("checksum"), "{log}");
    let after = Image::load(&path);
    assert_eq!(after.block(targets[0]), &copies[0][..]);
    assert_eq!(after.block(targets[1]), &copies[1][..], "escape undone");
    assert_eq!(
        after.block(targets[2]),
        &late[..],
        "logged again after the revoke"
    );
    assert!(
        after.block(targets[3]).iter().all(|&b| b == 0),
        "revoked by its own transaction"
    );
    assert_eq!(
        le32(after.sb(), soff::FEATURE_INCOMPAT) & incompat::RECOVER,
        0
    );
    let (code, log) = e2fsck(&path, "-n");
    assert_eq!(code, 0, "{log}");

    let mut ours = Image::load(&ours_path);
    let map = journal_map(&mut ours);
    let jsb = jbd2::Superblock::parse(ours.block(map[0])).unwrap();
    let total = sbk::blocks_count(ours.sb());
    let (mut meta, mut data) = (vec![0u8; bs], vec![0u8; bs]);
    let mut io = FileJournal {
        img: &mut ours,
        map,
        homes: BTreeSet::new(),
    };
    let done = recovery::recover(
        &mut io,
        &mut Revokes::default(),
        &jsb,
        &mut meta,
        &mut data,
        &|b| b < total,
    )
    .unwrap();
    assert_eq!(
        (done.transactions, done.blocks, done.next_sequence),
        (2, 3, seq + 3)
    );
    for &t in &targets {
        assert_eq!(ours.block(t), after.block(t), "block {t}");
    }
}

/// The shapes the kernel suite builds format; one that stopped would read
/// there as a skipped test rather than a failed one.
#[test]
fn the_kernel_suites_fixtures_format() {
    use slopos_ext4_core::fixture::{Spec, format};
    for journal_blocks in [0, 48] {
        let spec = Spec {
            block_size: 1024,
            blocks: 512,
            inodes: 64,
            inode_size: 256,
            extents: true,
            bit64: true,
            metadata_csum: true,
            journal_blocks,
            uuid: *b"slopos-ext4-test",
        };
        let mut image = vec![0u8; 512 * 1024];
        assert!(
            format(&mut image, &spec).is_ok(),
            "journal of {journal_blocks}"
        );
    }
}

/// The fixture volume passes `e2fsck` in each shape the kernel suite builds.
#[test]
fn fixtures_pass_e2fsck() {
    use slopos_ext4_core::fixture::{Spec, format};
    let s = Scratch::new();
    for (block_size, blocks, extents, bit64, csum, journal_blocks, inode_size) in [
        (4096u32, 8192u32, true, true, true, 1024u32, 256u16),
        (1024, 8192, true, true, true, 1024, 256),
        (1024, 4096, true, false, false, 1024, 256),
        (1024, 512, false, false, false, 0, 128),
        (4096, 2048, true, true, true, 0, 256),
    ] {
        let spec = Spec {
            block_size,
            blocks,
            inodes: 64,
            inode_size,
            extents,
            bit64,
            metadata_csum: csum,
            journal_blocks,
            uuid: [0x42; 16],
        };
        let mut image = vec![0u8; blocks as usize * block_size as usize];
        format(&mut image, &spec).unwrap();
        let path = s.path("fixture.img");
        std::fs::write(&path, &image).unwrap();
        let (code, log) = e2fsck(&path, "-n");
        assert_eq!(code, 0, "{spec:?}\n{log}");
    }
}
