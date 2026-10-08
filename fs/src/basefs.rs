//! The boot slot's base: a read-only filesystem over the `newc` archive the
//! boot module carries, served where it lies. A file's bytes are a slice of
//! the module, which the loader keeps mapped for the kernel's lifetime, so
//! the base costs an index and no copy. The install medium is a second
//! instance, over the `install` module.

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_ostd::KVec;
use slopos_ostd::sync::OnceLock;

use crate::MAX_NAME_LEN;
use crate::cpio::{CpioError, for_each_cpio_entry, nul_terminated};
use crate::vfs::traits::FsStats;
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

use slopos_abi::fs::BASEFS_MAGIC;

const ROOT: usize = 0;
/// How many nodes one chunk of the index holds, and how many entries one run
/// of its by-parent order: the kernel heap refuses a single allocation past
/// a megabyte, so no one allocation grows with the archive.
const CHUNK_SHIFT: u32 = 12;
const CHUNK: usize = 1 << CHUNK_SHIFT;

struct Node {
    parent: usize,
    name: &'static [u8],
    kind: FileType,
    mode: u16,
    data: &'static [u8],
}

/// Every node, in the order it was made, which is its inode order.
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

    fn key(&self, i: usize) -> (usize, &'static [u8]) {
        (self[i].parent, self[i].name)
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

    fn find(&self, nodes: &Nodes, key: (usize, &[u8])) -> Result<usize, Place> {
        let run = self.run_for(|i| nodes.key(i) < key);
        let Some(entries) = self.runs.get(run) else {
            let run = self.runs.len().saturating_sub(1);
            let at = self.runs.last().map_or(0, |entries| entries.len());
            return Err(Place { run, at });
        };
        entries
            .binary_search_by(|&i| nodes.key(i).cmp(&key))
            .map(|at| entries[at])
            .map_err(|at| Place { run, at })
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

    fn children<'a>(&'a self, nodes: &'a Nodes, dir: usize) -> impl Iterator<Item = usize> + 'a {
        let run = self.run_for(|i| nodes[i].parent < dir);
        let at = self.runs.get(run).map_or(0, |entries| {
            entries.partition_point(|&i| nodes[i].parent < dir)
        });
        self.runs
            .get(run..)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .flat_map(move |(k, entries)| entries[if k == 0 { at } else { 0 }..].iter().copied())
            .take_while(move |&i| nodes[i].parent == dir)
    }
}

struct Index {
    nodes: Nodes,
    by_parent: ByParent,
    archive_len: usize,
}

impl Index {
    fn build(archive: &'static [u8]) -> Result<Self, CpioError> {
        let mut index = Self {
            nodes: Nodes::default(),
            by_parent: ByParent::default(),
            archive_len: archive.len(),
        };
        index.nodes.push(Node {
            parent: ROOT,
            name: b"",
            kind: FileType::Directory,
            mode: 0o755,
            data: &[],
        })?;
        for_each_cpio_entry(archive, |entry| {
            let kind = match entry.mode & S_IFMT {
                S_IFDIR => FileType::Directory,
                S_IFREG => FileType::Regular,
                S_IFLNK => FileType::Symlink,
                _ => return Ok(()),
            };
            index.insert(entry.path, kind, (entry.mode & 0o7777) as u16, entry.data)
        })?;
        Ok(index)
    }

    /// Put `path` in, making the directories above it; a path the index
    /// holds already takes the new mode and bytes when the kinds agree.
    fn insert(
        &mut self,
        path: &'static [u8],
        kind: FileType,
        mode: u16,
        data: &'static [u8],
    ) -> Result<(), CpioError> {
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
            let implied = Node {
                parent,
                name,
                kind: FileType::Directory,
                mode: 0o755,
                data: &[],
            };
            parent = match (self.find(parent, name), last) {
                (Ok(i), false) if self.nodes[i].kind == FileType::Directory => i,
                (Ok(_), false) => return Err(CpioError::BadPath),
                (Err(place), false) => self.add(place, implied)?,
                (Ok(i), true) if self.nodes[i].kind != kind => {
                    return Err(CpioError::BadPath);
                }
                (Ok(i), true) => {
                    self.nodes[i].mode = mode;
                    self.nodes[i].data = data;
                    i
                }
                (Err(place), true) => self.add(
                    place,
                    Node {
                        kind,
                        mode,
                        data,
                        ..implied
                    },
                )?,
            };
        }
        Ok(())
    }

    fn holds(&self, path: &[u8]) -> bool {
        path.split(|&b| b == b'/')
            .filter(|name| !name.is_empty())
            .try_fold(ROOT, |dir, name| self.find(dir, name).ok())
            .is_some()
    }

    fn find(&self, parent: usize, name: &[u8]) -> Result<usize, Place> {
        self.by_parent.find(&self.nodes, (parent, name))
    }

    fn add(&mut self, place: Place, node: Node) -> Result<usize, CpioError> {
        let index = self.nodes.push(node)?;
        self.by_parent.insert(place, index)?;
        Ok(index)
    }

    fn node(&self, inode: InodeId) -> VfsResult<&Node> {
        let at = usize::try_from(inode.wrapping_sub(1)).map_err(|_| VfsError::NotFound)?;
        self.nodes.get(at).ok_or(VfsError::NotFound)
    }

    fn children(&self, dir: usize) -> impl Iterator<Item = usize> + '_ {
        self.by_parent.children(&self.nodes, dir)
    }
}

const fn inode_of(index: usize) -> InodeId {
    index as InodeId + 1
}

pub struct BaseFs {
    index: OnceLock<Index>,
}

/// The base the boot module carries, indexed by [`BaseFs::install`].
pub static BASE_FS: BaseFs = BaseFs::new();

/// The install medium: the `install` module's archive, beside the kernel and
/// the base the loader booted.
pub static MEDIUM_FS: BaseFs = BaseFs::new();

impl BaseFs {
    pub const fn new() -> Self {
        Self {
            index: OnceLock::new(),
        }
    }

    /// Index `archive` as this filesystem's contents, with each of `files`, a
    /// path and the bytes it reads as, beside what the archive holds; answers
    /// the entries it holds, and [`CpioError::Installed`] once a base is
    /// already indexed. A file the archive already holds is
    /// [`CpioError::BadPath`].
    pub fn install(
        &self,
        archive: &'static [u8],
        files: &[(&'static [u8], &'static [u8])],
    ) -> Result<usize, CpioError> {
        if self.index.get().is_some() {
            return Err(CpioError::Installed);
        }
        let mut index = Index::build(archive)?;
        for &(path, data) in files {
            if index.holds(path) {
                return Err(CpioError::BadPath);
            }
            index.insert(path, FileType::Regular, 0o644, data)?;
        }
        let entries = index.nodes.len - 1;
        self.index.call_once(move || index);
        Ok(entries)
    }

    fn index(&self) -> VfsResult<&Index> {
        self.index.get().ok_or(VfsError::NotFound)
    }

    /// The inode at absolute `path`.
    pub fn resolve(&self, path: &[u8]) -> VfsResult<InodeId> {
        path.split(|&b| b == b'/')
            .filter(|name| !name.is_empty())
            .try_fold(self.root_inode(), |dir, name| self.lookup(dir, name))
    }

    /// The bytes of the regular file at `path`, from the root, where they
    /// lie.
    pub fn file(&self, path: &[u8]) -> VfsResult<&'static [u8]> {
        let node = self.index()?.node(self.resolve(path)?)?;
        match node.kind {
            FileType::Regular => Ok(node.data),
            FileType::Directory => Err(VfsError::IsDirectory),
            _ => Err(VfsError::InvalidArgument),
        }
    }
}

impl Default for BaseFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for BaseFs {
    fn name(&self) -> &'static str {
        "basefs"
    }

    fn root_inode(&self) -> InodeId {
        inode_of(ROOT)
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        let index = self.index()?;
        let dir = index.node(parent)?;
        if dir.kind != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }
        let at = (parent - 1) as usize;
        match name {
            b"." => Ok(parent),
            b".." => Ok(inode_of(dir.parent)),
            _ => index
                .find(at, name)
                .map(inode_of)
                .map_err(|_| VfsError::NotFound),
        }
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let node = self.index()?.node(inode)?;
        let mut stat = match node.kind {
            FileType::Directory => FileStat::new_directory(inode),
            _ => FileStat::new_file(inode, node.data.len() as u64),
        };
        stat.file_type = node.kind;
        stat.mode = node.mode;
        stat.sealed = true;
        if node.kind == FileType::Symlink {
            stat.size = nul_terminated(node.data).len() as u64;
        }
        Ok(stat)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let node = self.index()?.node(inode)?;
        match node.kind {
            FileType::Regular => {}
            FileType::Directory => return Err(VfsError::IsDirectory),
            _ => return Err(VfsError::InvalidArgument),
        }
        let start = usize::try_from(offset).map_or(node.data.len(), |o| o.min(node.data.len()));
        let bytes = &node.data[start..];
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn write(&self, _inode: InodeId, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::ReadOnly)
    }

    fn create(&self, _parent: InodeId, _name: &[u8], _file_type: FileType) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let index = self.index()?;
        let dir = index.node(inode)?;
        if dir.kind != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }
        let dots = [
            (&b"."[..], inode, FileType::Directory),
            (&b".."[..], inode_of(dir.parent), FileType::Directory),
        ];
        let entries = index
            .children((inode - 1) as usize)
            .map(|i| (index.nodes[i].name, inode_of(i), index.nodes[i].kind));
        let mut visited = 0;
        for (name, ino, kind) in dots.into_iter().chain(entries).skip(offset) {
            if !callback(name, ino, kind) {
                break;
            }
            visited += 1;
        }
        Ok(visited)
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let node = self.index()?.node(inode)?;
        if node.kind != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        let target = nul_terminated(node.data);
        let n = target.len().min(buf.len());
        buf[..n].copy_from_slice(&target[..n]);
        Ok(n)
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn set_sealed(&self, _inode: InodeId) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self) -> VfsResult<FsStats> {
        let index = self.index()?;
        Ok(FsStats {
            magic: BASEFS_MAGIC,
            block_size: 4096,
            blocks: index.archive_len.div_ceil(4096) as u64,
            blocks_free: 0,
            blocks_available: 0,
            inodes: index.nodes.len as u64,
            inodes_free: 0,
            max_name_len: MAX_NAME_LEN as u32,
            read_only: true,
        })
    }
}
