//! The boot slot's base: a read-only filesystem over the `newc` archive the
//! boot module carries, served where it lies. A file's bytes are a slice of
//! the module, which the loader keeps mapped for the kernel's lifetime, so
//! the base costs an index and no copy.

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_ostd::KVec;
use slopos_ostd::sync::OnceLock;

use crate::MAX_NAME_LEN;
use crate::cpio::{CpioError, for_each_cpio_entry, nul_terminated};
use crate::vfs::traits::FsStats;
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

/// `f_type` in `statfs(2)`: the base is no filesystem Linux names.
pub const BASEFS_MAGIC: u64 = 0x534c_4253;

const ROOT: usize = 0;

struct Node {
    parent: usize,
    name: &'static [u8],
    kind: FileType,
    mode: u16,
    data: &'static [u8],
}

struct Index {
    nodes: KVec<Node>,
    /// Every node but the root, by parent and then name: a directory's
    /// entries are one run of it.
    by_parent: KVec<usize>,
    archive_len: usize,
}

impl Index {
    fn build(archive: &'static [u8]) -> Result<Self, CpioError> {
        let mut index = Self {
            nodes: KVec::new(),
            by_parent: KVec::new(),
            archive_len: archive.len(),
        };
        index
            .nodes
            .push(Node {
                parent: ROOT,
                name: b"",
                kind: FileType::Directory,
                mode: 0o755,
                data: &[],
            })
            .map_err(|_| CpioError::NoMemory)?;
        for_each_cpio_entry(archive, |entry| {
            let kind = match entry.mode & S_IFMT {
                S_IFDIR => FileType::Directory,
                S_IFREG => FileType::Regular,
                S_IFLNK => FileType::Symlink,
                _ => return Ok(()),
            };
            let mode = (entry.mode & 0o7777) as u16;
            let mut parent = ROOT;
            let mut names = entry
                .path
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
                parent = match (index.find(parent, name), last) {
                    (Ok(i), false) if index.nodes[i].kind == FileType::Directory => i,
                    (Ok(_), false) => return Err(CpioError::BadPath),
                    (Err(slot), false) => index.add(slot, implied)?,
                    (Ok(i), true) if index.nodes[i].kind != kind => {
                        return Err(CpioError::BadPath);
                    }
                    (Ok(i), true) => {
                        index.nodes[i].mode = mode;
                        index.nodes[i].data = entry.data;
                        i
                    }
                    (Err(slot), true) => index.add(
                        slot,
                        Node {
                            kind,
                            mode,
                            data: entry.data,
                            ..implied
                        },
                    )?,
                };
            }
            Ok(())
        })?;
        Ok(index)
    }

    /// The node `parent` holds as `name`, or where in `by_parent` it would go.
    fn find(&self, parent: usize, name: &[u8]) -> Result<usize, usize> {
        let by_parent = self.by_parent.as_slice();
        by_parent
            .binary_search_by(|&i| (self.nodes[i].parent, self.nodes[i].name).cmp(&(parent, name)))
            .map(|slot| by_parent[slot])
    }

    fn add(&mut self, slot: usize, node: Node) -> Result<usize, CpioError> {
        self.nodes.push(node).map_err(|_| CpioError::NoMemory)?;
        let index = self.nodes.len() - 1;
        self.by_parent
            .insert(slot, index)
            .map_err(|_| CpioError::NoMemory)?;
        Ok(index)
    }

    fn node(&self, inode: InodeId) -> VfsResult<&Node> {
        let at = usize::try_from(inode.wrapping_sub(1)).map_err(|_| VfsError::NotFound)?;
        self.nodes.as_slice().get(at).ok_or(VfsError::NotFound)
    }

    fn children(&self, dir: usize) -> &[usize] {
        let all = self.by_parent.as_slice();
        let start = all.partition_point(|&i| self.nodes[i].parent < dir);
        let len = all[start..].partition_point(|&i| self.nodes[i].parent == dir);
        &all[start..start + len]
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

impl BaseFs {
    pub const fn new() -> Self {
        Self {
            index: OnceLock::new(),
        }
    }

    /// Index `archive` as this filesystem's contents; answers the entries it
    /// holds, and [`CpioError::Installed`] once a base is already indexed.
    pub fn install(&self, archive: &'static [u8]) -> Result<usize, CpioError> {
        if self.index.get().is_some() {
            return Err(CpioError::Installed);
        }
        let index = Index::build(archive)?;
        let entries = index.nodes.len() - 1;
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
            .iter()
            .map(|&i| (index.nodes[i].name, inode_of(i), index.nodes[i].kind));
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
            inodes: index.nodes.len() as u64,
            inodes_free: 0,
            max_name_len: MAX_NAME_LEN as u32,
            read_only: true,
        })
    }
}
