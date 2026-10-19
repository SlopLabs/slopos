//! Where each directory and file of an activation lies in its tree, from one
//! walk at activation, so a program of it is named by its path. The walk
//! refuses a volume a path could not name a program of: an inode at two
//! names, a name no path holds or held twice in a directory, a `.` or `..`
//! that lies, an entry whose type is not its inode's, anything but
//! directories, regular files and symlinks, and directories larger together
//! than the volume, which only blocks read more than once can make.

use slopos_abi::extension::MAX_ENTRIES;
use slopos_ostd::KVec;

use crate::vfs::{FileSystem, FileType, InodeId, VfsError, VfsResult};

const NODES: usize = 4096;
/// The first inode no ext4 volume reserves, `lost+found` on one `mke2fs` or
/// the extension writer made.
pub(super) const FIRST_INO: u32 = 11;
const NAMES: usize = 64 * 1024;

/// FNV-1a: names that differ seldom share it, and those that do are
/// ordered by their bytes.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &b| {
        (hash ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// A node's place under a key, sorted as one word.
fn keyed(key: u32, place: usize) -> u64 {
    (u64::from(key) << 32) | place as u64
}

fn key(keyed: u64) -> u32 {
    (keyed >> 32) as u32
}

fn place(keyed: u64) -> usize {
    keyed as u32 as usize
}

/// Heapsort by `after`: no room beside the slice and no recursion, so every
/// frame of it is small.
fn sort(keys: &mut [u64], after: &dyn Fn(u64, u64) -> bool) {
    let sift = |keys: &mut [u64], mut root: usize, end: usize| loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && after(keys[child + 1], keys[child]) {
            child += 1;
        }
        if !after(keys[child], keys[root]) {
            return;
        }
        keys.swap(root, child);
        root = child;
    };
    for root in (0..keys.len() / 2).rev() {
        sift(keys, root, keys.len());
    }
    for end in (1..keys.len()).rev() {
        keys.swap(0, end);
        sift(keys, 0, end);
    }
}

#[derive(Clone, Copy)]
struct Node {
    ino: u32,
    parent: u32,
    chunk: u16,
    at: u16,
    len: u8,
    kind: FileType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    Read(VfsError),
    /// What the volume holds that no path names alone.
    Shape(&'static str),
    TooMany,
    NoMemory,
}

pub(super) struct Index {
    nodes: KVec<KVec<Node>>,
    names: KVec<KVec<u8>>,
    /// Every node's place keyed by its inode, in inode order.
    by_ino: KVec<u64>,
}

impl Index {
    /// Walk the volume `fs` serves from `root`, `bytes` long.
    #[inline(never)]
    pub(super) fn build(fs: &dyn FileSystem, root: u32, bytes: u64) -> Result<Self, IndexError> {
        let mut index = Self {
            nodes: KVec::new(),
            names: KVec::new(),
            by_ino: KVec::new(),
        };
        index.push(root, root, b"", FileType::Directory)?;
        let mut next = 0;
        let mut listed = 0u64;
        while next < index.len() {
            let dir = index.node(next);
            next += 1;
            if dir.kind != FileType::Directory {
                continue;
            }
            let size = fs.stat(dir.ino.into()).map_err(IndexError::Read)?.size;
            listed = listed.saturating_add(size);
            if listed > bytes {
                return Err(IndexError::Shape("directories larger than the volume"));
            }
            index.list(fs, dir)?;
        }
        let mut order = KVec::with_capacity(index.len()).map_err(|_| IndexError::NoMemory)?;
        for place in 0..index.len() {
            let ino = index.node(place).ino;
            order
                .push(keyed(ino, place))
                .map_err(|_| IndexError::NoMemory)?;
        }
        sort(&mut order, &|a, b| a > b);
        if order.windows(2).any(|pair| key(pair[0]) == key(pair[1])) {
            return Err(IndexError::Shape("an inode at two names"));
        }
        index.by_ino = order;
        Ok(index)
    }

    fn len(&self) -> usize {
        self.nodes.iter().map(KVec::len).sum()
    }

    fn node(&self, place: usize) -> Node {
        self.nodes[place / NODES][place % NODES]
    }

    fn name(&self, node: Node) -> &[u8] {
        let at = usize::from(node.at);
        &self.names[usize::from(node.chunk)][at..at + usize::from(node.len)]
    }

    fn push(
        &mut self,
        ino: u32,
        parent: u32,
        name: &[u8],
        kind: FileType,
    ) -> Result<(), IndexError> {
        if self.len() > MAX_ENTRIES {
            return Err(IndexError::TooMany);
        }
        let len = u8::try_from(name.len()).map_err(|_| IndexError::Shape("a name too long"))?;
        if self
            .names
            .last()
            .is_none_or(|chunk| chunk.len() + name.len() > NAMES)
        {
            self.names
                .push(KVec::new())
                .map_err(|_| IndexError::NoMemory)?;
        }
        let chunk = self.names.len() - 1;
        let names = &mut self.names[chunk];
        let at = names.len();
        names
            .extend_from_slice(name)
            .map_err(|_| IndexError::NoMemory)?;
        if self.nodes.last().is_none_or(|nodes| nodes.len() == NODES) {
            self.nodes
                .push(KVec::new())
                .map_err(|_| IndexError::NoMemory)?;
        }
        let node = Node {
            ino,
            parent,
            chunk: chunk as u16,
            at: at as u16,
            len,
            kind,
        };
        self.nodes
            .last_mut()
            .ok_or(IndexError::NoMemory)?
            .push(node)
            .map_err(|_| IndexError::NoMemory)
    }

    /// Index what `dir` holds.
    fn list(&mut self, fs: &dyn FileSystem, dir: Node) -> Result<(), IndexError> {
        let first = self.len();
        let mut failed = None;
        let mut dots = (None, None);
        fs.readdir(InodeId::from(dir.ino), 0, &mut |name, ino, kind| {
            let refused = match name {
                b"." if dots.0.is_none() => {
                    dots.0 = Some(ino);
                    None
                }
                b".." if dots.1.is_none() => {
                    dots.1 = Some(ino);
                    None
                }
                b"." | b".." => Some(IndexError::Shape("a directory that holds a name twice")),
                _ if name.is_empty() || name.contains(&b'/') || name.contains(&0) => {
                    Some(IndexError::Shape("a name no path holds"))
                }
                _ => match u32::try_from(ino) {
                    Ok(child) if child >= FIRST_INO => self.push(child, dir.ino, name, kind).err(),
                    _ => Some(IndexError::Shape("an entry naming a reserved inode")),
                },
            };
            failed = failed.or(refused);
            failed.is_none()
        })
        .map_err(IndexError::Read)?;
        if let Some(e) = failed {
            return Err(e);
        }
        if dots != (Some(dir.ino.into()), Some(dir.parent.into())) {
            return Err(IndexError::Shape("a directory whose `.` or `..` lies"));
        }
        for place in first..self.len() {
            let child = self.node(place);
            let kind = fs
                .stat(child.ino.into())
                .map_err(IndexError::Read)?
                .file_type;
            let served = matches!(
                kind,
                FileType::Directory | FileType::Regular | FileType::Symlink
            );
            if !served || kind != child.kind {
                return Err(IndexError::Shape(
                    "an entry that is no directory, file or symlink, or names another type",
                ));
            }
        }
        let mut names =
            KVec::with_capacity(self.len() - first).map_err(|_| IndexError::NoMemory)?;
        for place in first..self.len() {
            let hash = fnv1a(self.name(self.node(place)));
            names
                .push(keyed(hash, place))
                .map_err(|_| IndexError::NoMemory)?;
        }
        let name_of = |keyed: u64| self.name(self.node(place(keyed)));
        sort(&mut names, &|a, b| {
            (key(a), name_of(a)) > (key(b), name_of(b))
        });
        let twice = names
            .windows(2)
            .any(|pair| key(pair[0]) == key(pair[1]) && name_of(pair[0]) == name_of(pair[1]));
        if twice {
            return Err(IndexError::Shape("a directory that holds a name twice"));
        }
        Ok(())
    }

    fn find(&self, ino: u32) -> Option<Node> {
        let at = self
            .by_ino
            .binary_search_by_key(&ino, |&keyed| key(keyed))
            .ok()?;
        Some(self.node(place(self.by_ino[at])))
    }

    /// Write `ino`'s path from the volume's root into `buf` when it fits,
    /// and answer its length either way.
    pub(super) fn path_of(&self, ino: u32, buf: &mut [u8]) -> VfsResult<usize> {
        let start = self.find(ino).ok_or(VfsError::NotFound)?;
        let parent = |node: Node| self.find(node.parent).ok_or(VfsError::NotFound);
        let mut len = 0;
        let mut at = start;
        while at.len > 0 {
            len += 1 + usize::from(at.len);
            at = parent(at)?;
        }
        let len = len.max(1);
        if let Some(out) = buf.get_mut(..len) {
            out[0] = b'/';
            let mut end = len;
            let mut at = start;
            while at.len > 0 {
                let name = self.name(at);
                out[end - name.len()..end].copy_from_slice(name);
                end -= name.len() + 1;
                out[end] = b'/';
                at = parent(at)?;
            }
        }
        Ok(len)
    }
}
