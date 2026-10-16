//! One generation's index: its nodes in the order they were made, which is
//! their inode order, and every node but the root by parent and name.

use slopos_ostd::KVec;

use super::store::Span;
use crate::MAX_NAME_LEN;
use crate::cpio::CpioError;
use crate::vfs::FileType;

pub(super) const ROOT: usize = 0;
/// How many nodes one chunk of the index holds, and how many entries one run
/// of its by-parent order: the kernel heap refuses a single allocation past
/// a megabyte, so no one allocation grows with the archive.
const CHUNK_SHIFT: u32 = 12;
const CHUNK: usize = 1 << CHUNK_SHIFT;
const NAME_CHUNK: usize = 16 * 1024;

/// Where a node's bytes lie: in memory the loader keeps mapped for good, or
/// in the generation's own store.
#[derive(Clone, Copy)]
pub(super) enum Data {
    Static(&'static [u8]),
    Owned(Span),
}

impl Data {
    pub(super) const NONE: Self = Self::Static(&[]);
}

/// What a path names, as an archive record gives it.
#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub(super) kind: FileType,
    pub(super) mode: u16,
    pub(super) data: Data,
    /// A regular file's length; a symlink's target's, up to a NUL.
    pub(super) size: u64,
}

impl Entry {
    const IMPLIED_DIR: Self = Self {
        kind: FileType::Directory,
        mode: 0o755,
        data: Data::NONE,
        size: 0,
    };
}

pub(super) struct Node {
    pub(super) parent: usize,
    name: Name,
    pub(super) kind: FileType,
    pub(super) mode: u16,
    pub(super) data: Data,
    pub(super) size: u64,
}

#[derive(Clone, Copy)]
struct Name {
    chunk: u32,
    at: u32,
    len: u32,
}

/// Every name, each whole within one chunk.
#[derive(Default)]
struct Names {
    chunks: KVec<KVec<u8>>,
}

impl Names {
    fn add(&mut self, name: &[u8]) -> Result<Name, CpioError> {
        if name.is_empty() {
            return Ok(Name {
                chunk: 0,
                at: 0,
                len: 0,
            });
        }
        let fits = self
            .chunks
            .last()
            .is_some_and(|chunk| chunk.len() + name.len() <= NAME_CHUNK);
        if !fits {
            let chunk = KVec::with_capacity(NAME_CHUNK).map_err(|_| CpioError::NoMemory)?;
            self.chunks.push(chunk).map_err(|_| CpioError::NoMemory)?;
        }
        let chunk_at = self.chunks.len() - 1;
        let chunk = &mut self.chunks[chunk_at];
        let at = chunk.len();
        chunk
            .extend_from_slice(name)
            .map_err(|_| CpioError::NoMemory)?;
        Ok(Name {
            chunk: chunk_at as u32,
            at: at as u32,
            len: name.len() as u32,
        })
    }

    fn get(&self, name: Name) -> &[u8] {
        if name.len == 0 {
            return &[];
        }
        let at = name.at as usize;
        &self.chunks[name.chunk as usize][at..at + name.len as usize]
    }
}

#[derive(Default)]
struct Nodes {
    chunks: KVec<KVec<Node>>,
    len: usize,
}

impl Nodes {
    fn get(&self, i: usize) -> Option<&Node> {
        self.chunks.get(i >> CHUNK_SHIFT)?.get(i & (CHUNK - 1))
    }

    fn push(&mut self, node: Node) -> Result<usize, CpioError> {
        if self.len & (CHUNK - 1) == 0 {
            self.chunks
                .push(KVec::new())
                .map_err(|_| CpioError::NoMemory)?;
        }
        let chunk = self.chunks.last_mut().ok_or(CpioError::NoMemory)?;
        chunk.push(node).map_err(|_| CpioError::NoMemory)?;
        self.len += 1;
        Ok(self.len - 1)
    }
}

impl core::ops::Index<usize> for Nodes {
    type Output = Node;

    fn index(&self, i: usize) -> &Node {
        &self.chunks[i >> CHUNK_SHIFT][i & (CHUNK - 1)]
    }
}

impl core::ops::IndexMut<usize> for Nodes {
    fn index_mut(&mut self, i: usize) -> &mut Node {
        &mut self.chunks[i >> CHUNK_SHIFT][i & (CHUNK - 1)]
    }
}

/// Where in [`ByParent`] a node is, or would go.
#[derive(Clone, Copy)]
struct Place {
    run: usize,
    at: usize,
}

/// Every node but the root, by parent and then name, in sorted runs of at
/// most [`CHUNK`]: a directory's entries are one stretch of it.
#[derive(Default)]
struct ByParent {
    runs: KVec<KVec<usize>>,
}

impl ByParent {
    /// The first run whose last entry `below` does not hold for.
    fn run_for(&self, below: impl Fn(usize) -> bool) -> usize {
        self.runs
            .partition_point(|run| run.last().is_some_and(|&last| below(last)))
    }

    /// Put `node` at `place`, halving a full run first.
    fn insert(&mut self, place: Place, node: usize) -> Result<(), CpioError> {
        if self.runs.is_empty() {
            self.runs
                .push(KVec::new())
                .map_err(|_| CpioError::NoMemory)?;
        }
        let mut place = place;
        if self.runs[place.run].len() == CHUNK {
            let half = CHUNK / 2;
            let mut upper = KVec::new();
            upper
                .extend_from_slice(&self.runs[place.run][half..])
                .map_err(|_| CpioError::NoMemory)?;
            self.runs.try_reserve(1).map_err(|_| CpioError::NoMemory)?;
            self.runs[place.run].truncate(half);
            self.runs
                .insert(place.run + 1, upper)
                .map_err(|_| CpioError::NoMemory)?;
            if place.at > half {
                place = Place {
                    run: place.run + 1,
                    at: place.at - half,
                };
            }
        }
        self.runs[place.run]
            .insert(place.at, node)
            .map_err(|_| CpioError::NoMemory)
    }
}

#[derive(Default)]
pub(super) struct Index {
    nodes: Nodes,
    names: Names,
    by_parent: ByParent,
}

impl Index {
    pub(super) fn new() -> Result<Self, CpioError> {
        let mut index = Self::default();
        index.nodes.push(Node {
            parent: ROOT,
            name: Name {
                chunk: 0,
                at: 0,
                len: 0,
            },
            kind: FileType::Directory,
            mode: 0o755,
            data: Data::NONE,
            size: 0,
        })?;
        Ok(index)
    }

    pub(super) fn len(&self) -> usize {
        self.nodes.len
    }

    pub(super) fn name(&self, node: &Node) -> &[u8] {
        self.names.get(node.name)
    }

    fn key(&self, i: usize) -> (usize, &[u8]) {
        (self.nodes[i].parent, self.name(&self.nodes[i]))
    }

    /// Put `path` in, making the directories above it; a path the index
    /// holds already takes the new mode and bytes when the kinds agree.
    pub(super) fn insert(&mut self, path: &[u8], entry: Entry) -> Result<(), CpioError> {
        let mut parent = ROOT;
        let mut names = path
            .split(|&b| b == b'/')
            .filter(|name| !name.is_empty() && *name != b".")
            .peekable();
        while let Some(name) = names.next() {
            if name.len() > MAX_NAME_LEN {
                return Err(CpioError::NameTooLong);
            }
            if name == b".." {
                return Err(CpioError::BadPath);
            }
            let last = names.peek().is_none();
            parent = match (self.find(parent, name), last) {
                (Ok(i), false) if self.nodes[i].kind == FileType::Directory => i,
                (Ok(_), false) => return Err(CpioError::BadPath),
                (Err(place), false) => self.add(place, parent, name, Entry::IMPLIED_DIR)?,
                (Ok(i), true) if self.nodes[i].kind != entry.kind => {
                    return Err(CpioError::BadPath);
                }
                (Ok(i), true) => {
                    let node = &mut self.nodes[i];
                    node.mode = entry.mode;
                    node.data = entry.data;
                    node.size = entry.size;
                    i
                }
                (Err(place), true) => self.add(place, parent, name, entry)?,
            };
        }
        Ok(())
    }

    pub(super) fn resolve(&self, path: &[u8]) -> Option<usize> {
        path.split(|&b| b == b'/')
            .filter(|name| !name.is_empty())
            .try_fold(ROOT, |dir, name| self.find(dir, name).ok())
    }

    fn find(&self, parent: usize, name: &[u8]) -> Result<usize, Place> {
        let key = (parent, name);
        let run = self.by_parent.run_for(|i| self.key(i) < key);
        let Some(entries) = self.by_parent.runs.get(run) else {
            let run = self.by_parent.runs.len().saturating_sub(1);
            let at = self
                .by_parent
                .runs
                .last()
                .map_or(0, |entries| entries.len());
            return Err(Place { run, at });
        };
        entries
            .binary_search_by(|&i| self.key(i).cmp(&key))
            .map(|at| entries[at])
            .map_err(|at| Place { run, at })
    }

    pub(super) fn child(&self, parent: usize, name: &[u8]) -> Option<usize> {
        self.find(parent, name).ok()
    }

    fn add(
        &mut self,
        place: Place,
        parent: usize,
        name: &[u8],
        entry: Entry,
    ) -> Result<usize, CpioError> {
        let name = self.names.add(name)?;
        let index = self.nodes.push(Node {
            parent,
            name,
            kind: entry.kind,
            mode: entry.mode,
            data: entry.data,
            size: entry.size,
        })?;
        self.by_parent.insert(place, index)?;
        Ok(index)
    }

    pub(super) fn node(&self, at: usize) -> Option<&Node> {
        self.nodes.get(at)
    }

    pub(super) fn children(&self, dir: usize) -> impl Iterator<Item = usize> + '_ {
        let nodes = &self.nodes;
        let runs = &self.by_parent.runs;
        let run = self.by_parent.run_for(|i| nodes[i].parent < dir);
        let at = runs.get(run).map_or(0, |entries| {
            entries.partition_point(|&i| nodes[i].parent < dir)
        });
        runs.get(run..)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .flat_map(move |(k, entries)| entries[if k == 0 { at } else { 0 }..].iter().copied())
            .take_while(move |&i| nodes[i].parent == dir)
    }
}
