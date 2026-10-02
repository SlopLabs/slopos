//! FAT32 over a byte-addressed device, as an EFI system partition needs it:
//! format a volume, read any file, create files and directories under any
//! name, and replace a file without a window in which it is half written.
//!
//! A replacement is copy-on-write. The new contents go into clusters nothing
//! references, the FAT that chains them is written, and only then is the
//! directory entry pointed at them — one 32-byte store inside one sector,
//! which is the commit. The old chain is freed after it. A crash before the
//! commit leaves the old file and some lost clusters, a crash after it the
//! new file and some lost clusters; `fsck.fat` reclaims either, and neither
//! leaves a file that is partly one and partly the other. A bootloader
//! configuration that fits one sector is therefore replaced atomically, and so
//! is a kernel image.
//!
//! A name that fits 8.3, its case kept by the `DIR_NTRes` bits, is stored as a
//! short entry alone; any other gets long-name entries and a generated 8.3
//! alias, as the FAT specification has Windows make them, so `\EFI\SlopOS`
//! and `limine.conf` read back as given. Replacing a file keeps whatever names
//! it already had.
//!
//! The whole FAT is held in memory: an ESP of a few hundred megabytes has a
//! FAT of a few hundred kilobytes, and every allocation then costs no read.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// What the volume is stored on. Offsets and lengths are in bytes.
pub trait Device {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Error>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Error>;
    /// Order every write before this call ahead of every write after it.
    fn flush(&mut self) -> Result<(), Error>;
    fn size(&self) -> u64;
    /// The medium's logical block size, the smallest unit it writes whole.
    fn block_size(&self) -> u32 {
        512
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    /// Not a FAT32 volume, or one whose boot sector does not add up.
    NotFat32,
    /// A cluster chain or directory that points somewhere it cannot.
    Corrupt,
    NotFound,
    NotADirectory,
    IsADirectory,
    Exists,
    NoSpace,
    /// Empty, longer than 255 UTF-16 units, ending in a space or a period, or
    /// carrying a character a long name may not hold.
    InvalidName,
    DirectoryNotEmpty,
    /// Larger than a FAT file can be (4 GiB - 1), or a device too small to
    /// format.
    TooLarge,
    /// The volume's sectors are smaller than the device's logical blocks, so
    /// no sector could be written on its own.
    SectorTooSmall,
}

const SECTOR: usize = 512;
const ENTRY: usize = 32;
const FAT32_MIN_CLUSTERS: u32 = 65_525;
const FAT_ENTRY_MASK: u32 = 0x0FFF_FFFF;
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;
const FIRST_END_OF_CHAIN: u32 = 0x0FFF_FFF8;
const BAD_CLUSTER: u32 = 0x0FFF_FFF7;

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

/// `DIR_NTRes` bits: the base name, or the extension, displays lowercase.
const NTRES_LOWER_BASE: u8 = 0x08;
const NTRES_LOWER_EXT: u8 = 0x10;

const FREE_ENTRY: u8 = 0xE5;
/// A long name's UTF-16 units, at most.
const LONG_NAME_MAX: usize = 255;
/// UTF-16 units one long-name entry holds, and where in it each lies.
const LONG_UNITS: usize = 13;
const LONG_OFFSETS: [usize; LONG_UNITS] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
/// Ordinal bit of the long-name entry holding a name's last part.
const LAST_LONG_ENTRY: u8 = 0x40;
const END_OF_DIRECTORY: u8 = 0x00;

const FSINFO_LEAD: u32 = 0x4161_5252;
const FSINFO_STRUCT: u32 = 0x6141_7272;
const FSINFO_TRAIL: u32 = 0xAA55_0000;

#[derive(Clone, Copy, Debug)]
struct Geometry {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    fats: u32,
    fat_sectors: u32,
    root_cluster: u32,
    fsinfo_sector: u32,
    /// Clusters 2..cluster_count+2 exist.
    cluster_count: u32,
    /// Every FAT copy is written, or only `active_fat`.
    mirrored: bool,
    active_fat: u32,
}

impl Geometry {
    fn cluster_bytes(&self) -> usize {
        (self.bytes_per_sector * self.sectors_per_cluster) as usize
    }

    fn data_start(&self) -> u64 {
        u64::from(self.reserved_sectors + self.fats * self.fat_sectors)
            * u64::from(self.bytes_per_sector)
    }

    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_start() + u64::from(cluster - 2) * self.cluster_bytes() as u64
    }

    fn fat_offset(&self, copy: u32) -> u64 {
        u64::from(self.reserved_sectors + copy * self.fat_sectors)
            * u64::from(self.bytes_per_sector)
    }

    fn is_data_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster < self.cluster_count + 2
    }
}

/// One directory entry, with the names it is known by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The long name where there is one, else the short name as `NAME.EXT`.
    pub name: String,
    pub short_name: String,
    short: [u8; 11],
    pub is_dir: bool,
    pub size: u32,
    first_cluster: u32,
    /// Device offset of the short entry.
    short_at: u64,
    /// Device offsets of the long-name entries before it.
    long_at: Vec<u64>,
}

pub struct Volume<D: Device> {
    dev: D,
    g: Geometry,
    fat: Vec<u32>,
    /// One flag per FAT sector: whether memory differs from the medium.
    dirty: Vec<bool>,
    next_free: u32,
}

fn le16(b: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put16(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

impl<D: Device> Volume<D> {
    /// Mount the FAT32 volume `dev` holds.
    pub fn open(mut dev: D) -> Result<Self, Error> {
        let mut boot = [0u8; SECTOR];
        dev.read_at(0, &mut boot)?;
        if boot[510] != 0x55 || boot[511] != 0xAA {
            return Err(Error::NotFat32);
        }
        let bytes_per_sector = le16(&boot, 11);
        let sectors_per_cluster = u32::from(boot[13]);
        let reserved_sectors = le16(&boot, 14);
        let fats = u32::from(boot[16]);
        let root_entries = le16(&boot, 17);
        let total16 = le16(&boot, 19);
        let fat16_sectors = le16(&boot, 22);
        let total32 = le32(&boot, 32);
        let fat_sectors = le32(&boot, 36);
        let ext_flags = le16(&boot, 40);
        let root_cluster = le32(&boot, 44);
        let fsinfo_sector = le16(&boot, 48);
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
            || !sectors_per_cluster.is_power_of_two()
            || sectors_per_cluster > 128
            || reserved_sectors == 0
            || fats == 0
            || root_entries != 0
            || fat16_sectors != 0
            || fat_sectors == 0
        {
            return Err(Error::NotFat32);
        }
        if bytes_per_sector < dev.block_size() {
            return Err(Error::SectorTooSmall);
        }
        let total_sectors = if total16 != 0 { total16 } else { total32 };
        let meta = reserved_sectors
            .checked_add(fats.checked_mul(fat_sectors).ok_or(Error::NotFat32)?)
            .ok_or(Error::NotFat32)?;
        if total_sectors <= meta
            || u64::from(total_sectors) * u64::from(bytes_per_sector) > dev.size()
        {
            return Err(Error::NotFat32);
        }
        let cluster_count = (total_sectors - meta) / sectors_per_cluster;
        let fat_capacity = u64::from(fat_sectors) * u64::from(bytes_per_sector) / 4;
        if cluster_count < FAT32_MIN_CLUSTERS
            || u64::from(cluster_count) + 2 > fat_capacity
            || cluster_count > BAD_CLUSTER - 2
        {
            return Err(Error::NotFat32);
        }
        let mirrored = ext_flags & 0x80 == 0;
        let active_fat = if mirrored { 0 } else { ext_flags & 0x0F };
        if active_fat >= fats {
            return Err(Error::NotFat32);
        }
        let g = Geometry {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fats,
            fat_sectors,
            root_cluster,
            fsinfo_sector,
            cluster_count,
            mirrored,
            active_fat,
        };
        if !g.is_data_cluster(root_cluster) {
            return Err(Error::NotFat32);
        }

        let entries = cluster_count as usize + 2;
        let mut raw = vec![0u8; entries * 4];
        dev.read_at(g.fat_offset(active_fat), &mut raw)?;
        let fat = raw
            .chunks_exact(4)
            .map(|e| le32(e, 0) & FAT_ENTRY_MASK)
            .collect();
        let fat_sector_count = (entries * 4).div_ceil(bytes_per_sector as usize);
        let mut volume = Self {
            dev,
            g,
            fat,
            dirty: vec![false; fat_sector_count],
            next_free: 2,
        };
        volume.next_free = volume.read_next_free_hint().unwrap_or(2);
        Ok(volume)
    }

    /// Give the device back.
    pub fn into_device(self) -> D {
        self.dev
    }

    /// Clusters no chain holds.
    pub fn free_clusters(&self) -> u32 {
        self.fat[2..].iter().filter(|&&e| e == 0).count() as u32
    }

    pub fn cluster_bytes(&self) -> usize {
        self.g.cluster_bytes()
    }

    fn read_next_free_hint(&mut self) -> Option<u32> {
        if self.g.fsinfo_sector == 0 || self.g.fsinfo_sector >= self.g.reserved_sectors {
            return None;
        }
        let mut info = [0u8; SECTOR];
        let at = u64::from(self.g.fsinfo_sector) * u64::from(self.g.bytes_per_sector);
        self.dev.read_at(at, &mut info).ok()?;
        if le32(&info, 0) != FSINFO_LEAD || le32(&info, 484) != FSINFO_STRUCT {
            return None;
        }
        let hint = le32(&info, 492);
        self.g.is_data_cluster(hint).then_some(hint)
    }

    fn next(&self, cluster: u32) -> Result<Option<u32>, Error> {
        let e = *self.fat.get(cluster as usize).ok_or(Error::Corrupt)?;
        match e {
            e if e >= FIRST_END_OF_CHAIN => Ok(None),
            e if self.g.is_data_cluster(e) => Ok(Some(e)),
            _ => Err(Error::Corrupt),
        }
    }

    /// The chain from `first`, refusing one that loops or leaves the volume.
    fn chain(&self, first: u32) -> Result<Vec<u32>, Error> {
        let mut out = Vec::new();
        if first == 0 {
            return Ok(out);
        }
        if !self.g.is_data_cluster(first) {
            return Err(Error::Corrupt);
        }
        let mut cur = Some(first);
        while let Some(c) = cur {
            if out.len() > self.g.cluster_count as usize {
                return Err(Error::Corrupt);
            }
            out.push(c);
            cur = self.next(c)?;
        }
        Ok(out)
    }

    fn set_fat(&mut self, cluster: u32, value: u32) {
        let idx = cluster as usize;
        self.fat[idx] = value & FAT_ENTRY_MASK;
        self.dirty[idx * 4 / self.g.bytes_per_sector as usize] = true;
    }

    /// Chain `count` free clusters together, in memory only.
    fn allocate(&mut self, count: usize) -> Result<Vec<u32>, Error> {
        let mut got = Vec::with_capacity(count);
        let total = self.g.cluster_count;
        let mut probe = self.next_free;
        let mut scanned = 0u32;
        while got.len() < count {
            if scanned >= total {
                for &c in &got {
                    self.set_fat(c, 0);
                }
                return Err(Error::NoSpace);
            }
            if !self.g.is_data_cluster(probe) {
                probe = 2;
            }
            if self.fat[probe as usize] == 0 {
                got.push(probe);
                self.set_fat(probe, END_OF_CHAIN);
            }
            probe += 1;
            scanned += 1;
        }
        for pair in got.windows(2) {
            self.set_fat(pair[0], pair[1]);
        }
        self.next_free = if self.g.is_data_cluster(probe) {
            probe
        } else {
            2
        };
        Ok(got)
    }

    fn release(&mut self, chain: &[u32]) {
        for &c in chain {
            self.set_fat(c, 0);
        }
        if let Some(&first) = chain.first() {
            self.next_free = self.next_free.min(first);
        }
    }

    /// Write every FAT sector that differs from the medium, to every copy
    /// the volume keeps in step.
    fn write_fat(&mut self) -> Result<(), Error> {
        let bps = self.g.bytes_per_sector as usize;
        let per_sector = bps / 4;
        let mut sector = vec![0u8; bps];
        for s in 0..self.dirty.len() {
            if !self.dirty[s] {
                continue;
            }
            // Bits 28..32 are reserved and preserved from what is there.
            let base = s * per_sector;
            let at_active = self.g.fat_offset(self.g.active_fat) + (s * bps) as u64;
            self.dev.read_at(at_active, &mut sector)?;
            for i in 0..per_sector {
                if let Some(&v) = self.fat.get(base + i) {
                    let old = le32(&sector, i * 4);
                    put32(&mut sector, i * 4, (old & !FAT_ENTRY_MASK) | v);
                }
            }
            let copies = if self.g.mirrored {
                0..self.g.fats
            } else {
                self.g.active_fat..self.g.active_fat + 1
            };
            for copy in copies {
                self.dev
                    .write_at(self.g.fat_offset(copy) + (s * bps) as u64, &sector)?;
            }
            self.dirty[s] = false;
        }
        Ok(())
    }

    fn write_fsinfo(&mut self) -> Result<(), Error> {
        if self.g.fsinfo_sector == 0 || self.g.fsinfo_sector >= self.g.reserved_sectors {
            return Ok(());
        }
        let at = u64::from(self.g.fsinfo_sector) * u64::from(self.g.bytes_per_sector);
        let mut info = [0u8; SECTOR];
        self.dev.read_at(at, &mut info)?;
        if le32(&info, 0) != FSINFO_LEAD || le32(&info, 484) != FSINFO_STRUCT {
            return Ok(());
        }
        put32(&mut info, 488, self.free_clusters());
        put32(&mut info, 492, self.next_free);
        self.dev.write_at(at, &info)
    }

    fn read_cluster(&mut self, cluster: u32, buf: &mut [u8]) -> Result<(), Error> {
        let at = self.g.cluster_offset(cluster);
        self.dev.read_at(at, buf)
    }

    /// Every live entry of the directory whose chain starts at `first`.
    fn entries(&mut self, first: u32) -> Result<Vec<Entry>, Error> {
        let chain = self.chain(first)?;
        let cb = self.g.cluster_bytes();
        let mut buf = vec![0u8; cb];
        let mut out = Vec::new();
        let mut long = LongName::default();
        for &cluster in &chain {
            self.read_cluster(cluster, &mut buf)?;
            let base = self.g.cluster_offset(cluster);
            for (i, raw) in buf.chunks_exact(ENTRY).enumerate() {
                let at = base + (i * ENTRY) as u64;
                match raw[0] {
                    END_OF_DIRECTORY => return Ok(out),
                    FREE_ENTRY => {
                        long.clear();
                        continue;
                    }
                    _ => {}
                }
                let attr = raw[11];
                if attr & 0x3F == ATTR_LONG_NAME {
                    long.push(raw, at);
                    continue;
                }
                if attr & ATTR_VOLUME_ID != 0 {
                    long.clear();
                    continue;
                }
                let mut short = [0u8; 11];
                short.copy_from_slice(&raw[..11]);
                let short_name = display_short(&short, raw[12]);
                let (name, long_at) = match long.take(&short) {
                    Some((name, at)) => (name, at),
                    None => (short_name.clone(), Vec::new()),
                };
                out.push(Entry {
                    name,
                    short_name,
                    short,
                    is_dir: attr & ATTR_DIRECTORY != 0,
                    size: le32(raw, 28),
                    first_cluster: (le16(raw, 20) << 16) | le16(raw, 26),
                    short_at: at,
                    long_at,
                });
            }
        }
        Ok(out)
    }

    /// The first cluster of the directory `path` names; `/` is the root.
    fn dir_cluster(&mut self, path: &str) -> Result<u32, Error> {
        let mut cluster = self.g.root_cluster;
        for part in components(path) {
            let entry = self.find(cluster, part)?.ok_or(Error::NotFound)?;
            if !entry.is_dir {
                return Err(Error::NotADirectory);
            }
            // A `..` to the root reads as cluster 0.
            cluster = if entry.first_cluster == 0 {
                self.g.root_cluster
            } else {
                entry.first_cluster
            };
        }
        Ok(cluster)
    }

    fn find(&mut self, dir: u32, name: &str) -> Result<Option<Entry>, Error> {
        Ok(self
            .entries(dir)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name) || e.short_name.eq_ignore_ascii_case(name)))
    }

    fn split(path: &str) -> Result<(&str, &str), Error> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = match trimmed.rfind('/') {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        if name.is_empty() || name == "." || name == ".." {
            return Err(Error::InvalidName);
        }
        Ok((parent, name))
    }

    /// The entry `path` names.
    pub fn stat(&mut self, path: &str) -> Result<Entry, Error> {
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_cluster(parent)?;
        self.find(dir, name)?.ok_or(Error::NotFound)
    }

    /// The entries of the directory `path` names, `.` and `..` excepted.
    pub fn list(&mut self, path: &str) -> Result<Vec<Entry>, Error> {
        let dir = self.dir_cluster(path)?;
        Ok(self
            .entries(dir)?
            .into_iter()
            .filter(|e| e.short_name != "." && e.short_name != "..")
            .collect())
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let entry = self.stat(path)?;
        if entry.is_dir {
            return Err(Error::IsADirectory);
        }
        let size = entry.size as usize;
        let cb = self.g.cluster_bytes();
        let chain = self.chain(entry.first_cluster)?;
        if chain.len() < size.div_ceil(cb) {
            return Err(Error::Corrupt);
        }
        let mut out = vec![0u8; size];
        let mut done = 0;
        for (first, bytes) in extents(&chain, cb, size) {
            self.dev
                .read_at(self.g.cluster_offset(first), &mut out[done..done + bytes])?;
            done += bytes;
        }
        Ok(out)
    }

    /// Write `data` into a fresh chain; nothing references it yet.
    fn store(&mut self, data: &[u8]) -> Result<Vec<u32>, Error> {
        let cb = self.g.cluster_bytes();
        let chain = self.allocate(data.len().div_ceil(cb))?;
        let mut done = 0;
        for (first, bytes) in extents(&chain, cb, data.len()) {
            let at = self.g.cluster_offset(first);
            if let Err(e) = self.dev.write_at(at, &data[done..done + bytes]) {
                self.release(&chain);
                return Err(e);
            }
            done += bytes;
        }
        Ok(chain)
    }

    /// Create `path` holding `data`, or replace what it holds.
    ///
    /// Copy-on-write, so the file reads as entirely old or entirely new
    /// across a crash at any point: the directory entry's one-sector store is
    /// the commit, ordered after the data and the FAT by a device flush.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), Error> {
        let size = u32::try_from(data.len()).map_err(|_| Error::TooLarge)?;
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_cluster(parent)?;
        let existing = self.find(dir, name)?;
        let fresh = match &existing {
            Some(e) if e.is_dir => return Err(Error::IsADirectory),
            Some(_) => None,
            None => Some(self.new_name(dir, name)?),
        };

        let chain = self.store(data)?;
        let first = chain.first().copied().unwrap_or(0);
        self.write_fat()?;
        self.dev.flush()?;

        match existing {
            Some(old) => {
                let mut raw = [0u8; ENTRY];
                self.dev.read_at(old.short_at, &mut raw)?;
                put16(&mut raw, 20, first >> 16);
                put16(&mut raw, 26, first & 0xFFFF);
                put32(&mut raw, 28, size);
                raw[11] |= ATTR_ARCHIVE;
                self.dev.write_at(old.short_at, &raw)?;
                self.dev.flush()?;
                let old_chain = self.chain(old.first_cluster)?;
                self.release(&old_chain);
            }
            None => {
                let fresh = fresh.ok_or(Error::InvalidName)?;
                self.insert(dir, &fresh, ATTR_ARCHIVE, first, size)?;
            }
        }
        self.write_fat()?;
        self.write_fsinfo()?;
        self.dev.flush()
    }

    /// Create the directory `path`.
    pub fn create_dir(&mut self, path: &str) -> Result<(), Error> {
        let (parent_path, name) = Self::split(path)?;
        let parent = self.dir_cluster(parent_path)?;
        if self.find(parent, name)?.is_some() {
            return Err(Error::Exists);
        }
        let fresh = self.new_name(parent, name)?;
        let chain = self.allocate(1)?;
        let cluster = chain[0];
        let mut buf = vec![0u8; self.g.cluster_bytes()];
        let dot_parent = if parent == self.g.root_cluster {
            0
        } else {
            parent
        };
        write_short(
            &mut buf[..ENTRY],
            b".          ",
            0,
            ATTR_DIRECTORY,
            cluster,
            0,
        );
        write_short(
            &mut buf[ENTRY..2 * ENTRY],
            b"..         ",
            0,
            ATTR_DIRECTORY,
            dot_parent,
            0,
        );
        let at = self.g.cluster_offset(cluster);
        if let Err(e) = self.dev.write_at(at, &buf) {
            self.release(&chain);
            return Err(e);
        }
        self.write_fat()?;
        self.dev.flush()?;
        self.insert(parent, &fresh, ATTR_DIRECTORY, cluster, 0)?;
        self.write_fsinfo()?;
        self.dev.flush()
    }

    /// Remove the file or empty directory `path`.
    pub fn remove(&mut self, path: &str) -> Result<(), Error> {
        let entry = self.stat(path)?;
        if entry.is_dir {
            let inside = self.entries(entry.first_cluster)?;
            if inside
                .iter()
                .any(|e| e.short_name != "." && e.short_name != "..")
            {
                return Err(Error::DirectoryNotEmpty);
            }
        }
        for &at in entry
            .long_at
            .iter()
            .chain(core::iter::once(&entry.short_at))
        {
            self.dev.write_at(at, &[FREE_ENTRY])?;
        }
        self.dev.flush()?;
        let chain = self.chain(entry.first_cluster)?;
        self.release(&chain);
        self.write_fat()?;
        self.write_fsinfo()?;
        self.dev.flush()
    }

    /// The entries `name` takes in the directory at `dir`: a short entry
    /// alone when it fits 8.3, else long-name entries and an alias no entry
    /// there already has.
    fn new_name(&mut self, dir: u32, name: &str) -> Result<NewName, Error> {
        if let Ok((short, ntres)) = short_name(name) {
            return Ok(NewName {
                short,
                ntres,
                long: Vec::new(),
            });
        }
        let units = long_units(name)?;
        let taken: Vec<[u8; 11]> = self.entries(dir)?.into_iter().map(|e| e.short).collect();
        let short = alias(name, &taken)?;
        Ok(NewName {
            short,
            ntres: 0,
            long: long_entries(&units, short_checksum(&short)),
        })
    }

    /// Put `name`'s entries into the directory at `dir`, in a run of free
    /// slots, growing the directory when it has none long enough. The short
    /// entry is written last, behind a flush: a crash before it leaves only
    /// long-name entries no short entry claims, which every reader skips.
    fn insert(
        &mut self,
        dir: u32,
        name: &NewName,
        attr: u8,
        first: u32,
        size: u32,
    ) -> Result<(), Error> {
        let needed = name.long.len() + 1;
        let mut chain = self.chain(dir)?;
        let cb = self.g.cluster_bytes();
        let mut buf = vec![0u8; cb];
        let mut run: Vec<u64> = Vec::new();
        'search: for &cluster in &chain {
            self.read_cluster(cluster, &mut buf)?;
            for (i, raw) in buf.chunks_exact(ENTRY).enumerate() {
                if raw[0] == END_OF_DIRECTORY || raw[0] == FREE_ENTRY {
                    run.push(self.g.cluster_offset(cluster) + (i * ENTRY) as u64);
                    if run.len() == needed {
                        break 'search;
                    }
                } else {
                    run.clear();
                }
            }
        }
        if run.len() < needed {
            let more = ((needed - run.len()) * ENTRY).div_ceil(cb);
            let grown = self.allocate(more)?;
            let zero = vec![0u8; cb];
            for &cluster in &grown {
                self.dev.write_at(self.g.cluster_offset(cluster), &zero)?;
            }
            let last = *chain.last().ok_or(Error::Corrupt)?;
            self.set_fat(last, grown[0]);
            self.write_fat()?;
            self.dev.flush()?;
            chain.extend_from_slice(&grown);
            for cluster in grown {
                let base = self.g.cluster_offset(cluster);
                run.extend((0..cb / ENTRY).map(|i| base + (i * ENTRY) as u64));
            }
            run.truncate(needed);
        }
        for (&at, raw) in run.iter().zip(&name.long) {
            self.dev.write_at(at, raw)?;
        }
        if !name.long.is_empty() {
            self.dev.flush()?;
        }
        let mut raw = [0u8; ENTRY];
        write_short(&mut raw, &name.short, name.ntres, attr, first, size);
        self.dev.write_at(run[needed - 1], &raw)
    }
}

/// The directory entries a new name is stored as.
struct NewName {
    short: [u8; 11],
    ntres: u8,
    /// In the order they are stored: the last part of the name first.
    long: Vec<[u8; ENTRY]>,
}

/// Characters a long name may hold beyond those of an 8.3 one.
fn is_long_char(c: char) -> bool {
    !c.is_control() && !"\"*/:<>?\\|".contains(c)
}

fn long_units(name: &str) -> Result<Vec<u16>, Error> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.is_empty()
        || units.len() > LONG_NAME_MAX
        || name.ends_with(['.', ' '])
        || !name.chars().all(is_long_char)
    {
        return Err(Error::InvalidName);
    }
    Ok(units)
}

/// The 8.3 alias of a long name, as the specification's basis-name
/// algorithm makes it: uppercased, spaces and leading periods dropped, what an
/// 8.3 name cannot hold as `_`, the base up to the first period and the
/// extension after the last. That basis stands alone when it spells the whole
/// name and no entry in `taken` has it; otherwise it takes the lowest `~n` tail
/// none has.
fn alias(name: &str, taken: &[[u8; 11]]) -> Result<[u8; 11], Error> {
    let oem = |c: char| match u8::try_from(c.to_ascii_uppercase()) {
        Ok(b) if is_short_char(b) => b,
        _ => b'_',
    };
    let trimmed = name.trim_start_matches(['.', ' ']);
    let lossy = trimmed.len() != name.len()
        || trimmed
            .chars()
            .any(|c| c != '.' && oem(c) == b'_' && c != '_');
    let base: Vec<u8> = trimmed
        .chars()
        .take_while(|&c| c != '.')
        .filter(|&c| c != ' ')
        .map(oem)
        .collect();
    let ext: Vec<u8> = match trimmed.rfind('.') {
        Some(dot) => trimmed[dot + 1..]
            .chars()
            .filter(|&c| c != ' ')
            .map(oem)
            .collect(),
        None => Vec::new(),
    };
    let dots = trimmed.matches('.').count();
    let fits = !lossy && base.len() <= 8 && ext.len() <= 3 && dots <= 1;
    let (base, ext) = (&base[..base.len().min(8)], &ext[..ext.len().min(3)]);
    let compose = |tail: &[u8]| {
        let keep = base.len().min(8 - tail.len());
        let mut short = [b' '; 11];
        short[..keep].copy_from_slice(&base[..keep]);
        short[keep..keep + tail.len()].copy_from_slice(tail);
        short[8..8 + ext.len()].copy_from_slice(ext);
        if short[0] == FREE_ENTRY {
            short[0] = 0x05;
        }
        short
    };
    if fits && !taken.contains(&compose(&[])) {
        return Ok(compose(&[]));
    }
    for n in 1..=999_999u32 {
        let mut digits = [0u8; 7];
        let short = compose(format_tail(n, &mut digits));
        if !taken.contains(&short) {
            return Ok(short);
        }
    }
    Err(Error::Exists)
}

fn format_tail(mut n: u32, out: &mut [u8; 7]) -> &[u8] {
    let mut at = out.len();
    while n > 0 {
        at -= 1;
        out[at] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    at -= 1;
    out[at] = b'~';
    &out[at..]
}

/// The long-name entries holding `units`, 13 to an entry, the last part
/// first, each tagged with its ordinal and `checksum`, the alias's.
fn long_entries(units: &[u16], checksum: u8) -> Vec<[u8; ENTRY]> {
    let parts = units.len().div_ceil(LONG_UNITS);
    let mut out = Vec::with_capacity(parts);
    for part in (0..parts).rev() {
        let mut raw = [0u8; ENTRY];
        let ordinal = part as u8 + 1;
        raw[0] = if part + 1 == parts {
            ordinal | LAST_LONG_ENTRY
        } else {
            ordinal
        };
        raw[11] = ATTR_LONG_NAME;
        raw[13] = checksum;
        for (i, at) in LONG_OFFSETS.into_iter().enumerate() {
            let unit = match (part * LONG_UNITS + i).cmp(&units.len()) {
                core::cmp::Ordering::Less => units[part * LONG_UNITS + i],
                core::cmp::Ordering::Equal => 0x0000,
                core::cmp::Ordering::Greater => 0xFFFF,
            };
            raw[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.push(raw);
    }
    out
}

/// Split a path into its components, dropping empty ones and `.`.
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|p| !p.is_empty() && *p != ".")
}

fn write_short(raw: &mut [u8], short: &[u8; 11], ntres: u8, attr: u8, first: u32, size: u32) {
    raw[..ENTRY].fill(0);
    raw[..11].copy_from_slice(short);
    raw[11] = attr;
    raw[12] = ntres;
    // 1980-01-01 00:00, the epoch: nothing here keeps time.
    put16(raw, 16, 0x21);
    put16(raw, 18, 0x21);
    put16(raw, 20, first >> 16);
    put16(raw, 24, 0x21);
    put16(raw, 26, first & 0xFFFF);
    put32(raw, 28, size);
}

fn is_short_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"$%'-_@~`!(){}^#&".contains(&c)
}

/// The 8.3 form of `name`, and the `DIR_NTRes` bits that keep an all-lowercase
/// base or extension displaying as it was given.
fn short_name(name: &str) -> Result<([u8; 11], u8), Error> {
    let bytes = name.as_bytes();
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (bytes, &b""[..]),
    };
    if base.is_empty()
        || base.len() > 8
        || ext.len() > 3
        || !base.iter().chain(ext).all(|&c| is_short_char(c))
        || (name.contains('.') && ext.is_empty())
    {
        return Err(Error::InvalidName);
    }
    let mut short = [b' '; 11];
    for (i, &c) in base.iter().enumerate() {
        short[i] = c.to_ascii_uppercase();
    }
    for (i, &c) in ext.iter().enumerate() {
        short[8 + i] = c.to_ascii_uppercase();
    }
    let lower = |part: &[u8]| part.iter().any(u8::is_ascii_lowercase);
    let upper = |part: &[u8]| part.iter().any(u8::is_ascii_uppercase);
    if (lower(base) && upper(base)) || (lower(ext) && upper(ext)) {
        return Err(Error::InvalidName);
    }
    let mut ntres = 0;
    if lower(base) {
        ntres |= NTRES_LOWER_BASE;
    }
    if lower(ext) {
        ntres |= NTRES_LOWER_EXT;
    }
    if short[0] == FREE_ENTRY {
        short[0] = 0x05;
    }
    Ok((short, ntres))
}

fn display_short(short: &[u8; 11], ntres: u8) -> String {
    let mut out = String::new();
    let mut first = short[0];
    if first == 0x05 {
        first = FREE_ENTRY;
    }
    let base = core::iter::once(first).chain(short[1..8].iter().copied());
    for c in base.take_while(|&c| c != b' ') {
        let c = if ntres & NTRES_LOWER_BASE != 0 {
            c.to_ascii_lowercase()
        } else {
            c
        };
        out.push(char::from(c));
    }
    let ext: Vec<u8> = short[8..]
        .iter()
        .copied()
        .take_while(|&c| c != b' ')
        .collect();
    if !ext.is_empty() {
        out.push('.');
        for c in ext {
            let c = if ntres & NTRES_LOWER_EXT != 0 {
                c.to_ascii_lowercase()
            } else {
                c
            };
            out.push(char::from(c));
        }
    }
    out
}

fn short_checksum(short: &[u8; 11]) -> u8 {
    short
        .iter()
        .fold(0u8, |sum, &c| sum.rotate_right(1).wrapping_add(c))
}

/// Long-name entries gathered ahead of the short entry they describe. They
/// are stored last-first, each tagged with its ordinal and the short name's
/// checksum; a run that does not end in ordinal 1 or whose checksum does not
/// match is an orphan, and the short name stands alone.
#[derive(Default)]
struct LongName {
    parts: Vec<(u8, [u16; LONG_UNITS])>,
    at: Vec<u64>,
    checksum: u8,
    expected: u8,
}

impl LongName {
    fn clear(&mut self) {
        self.parts.clear();
        self.at.clear();
        self.expected = 0;
    }

    fn push(&mut self, raw: &[u8], at: u64) {
        let ord = raw[0] & 0x3F;
        if raw[0] & LAST_LONG_ENTRY != 0 {
            self.clear();
            self.checksum = raw[13];
            self.expected = ord;
        }
        if ord == 0 || ord != self.expected || raw[13] != self.checksum {
            self.clear();
            return;
        }
        let units = LONG_OFFSETS.map(|at| u16::from_le_bytes([raw[at], raw[at + 1]]));
        self.parts.push((ord, units));
        self.at.push(at);
        self.expected = ord - 1;
    }

    fn take(&mut self, short: &[u8; 11]) -> Option<(String, Vec<u64>)> {
        let complete =
            !self.parts.is_empty() && self.expected == 0 && self.checksum == short_checksum(short);
        if !complete {
            self.clear();
            return None;
        }
        let mut units = Vec::new();
        for (_, part) in self.parts.iter().rev() {
            for &u in part {
                if u == 0x0000 || u == 0xFFFF {
                    break;
                }
                units.push(u);
            }
        }
        let name: String = char::decode_utf16(units)
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        let at = core::mem::take(&mut self.at);
        self.clear();
        Some((name, at))
    }
}

/// Reserved sectors a volume starts with at least: the boot sector, FSInfo and
/// their backups at 6 and 7, as the specification lays them out.
const MIN_RESERVED: u32 = 32;
const FATS: u32 = 2;
/// Where a volume's data region starts, from the volume's own start: the
/// boundary partitions are placed on, so each cluster lies inside one
/// physical block and one transfer unit, as UEFI asks of an ESP (§13.3.1.1).
const DATA_ALIGN_BYTES: u64 = 1 << 20;
/// The most `BPB_RsvdSecCnt` holds.
const MAX_RESERVED: u32 = u16::MAX as u32;

/// The largest cluster up to 4 KiB that leaves FAT32's minimum cluster count
/// over `total` sectors after `reserved`, and the FAT that describes them.
fn fat_layout(total: u32, reserved: u32, sector: usize) -> Result<Option<(u32, u32)>, Error> {
    for spc in [8u32, 4, 2, 1]
        .into_iter()
        .filter(|&spc| spc as usize * sector <= 4096)
    {
        // Fixpoint: the FAT's own size eats the space it describes.
        let mut fat_sectors = 1u32;
        loop {
            let data = total.saturating_sub(reserved + FATS * fat_sectors);
            let clusters = data / spc;
            let need = u32::try_from((u64::from(clusters) + 2) * 4)
                .map_err(|_| Error::TooLarge)?
                .div_ceil(sector as u32);
            if need <= fat_sectors {
                if clusters >= FAT32_MIN_CLUSTERS {
                    return Ok(Some((spc, fat_sectors)));
                }
                break;
            }
            fat_sectors = need;
        }
    }
    Ok(None)
}

fn zero_fill<D: Device>(dev: &mut D, offset: u64, len: u64) -> Result<(), Error> {
    let zero = vec![0u8; MAX_EXTENT];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(MAX_EXTENT as u64) as usize;
        dev.write_at(offset + done, &zero[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Lay a fresh FAT32 volume over the first `bytes` of `dev`: sectors of the
/// device's logical block size, two FATs, the largest cluster up to 4 KiB
/// that still leaves FAT32's minimum cluster count, the reserved region grown
/// until the data region starts on [`DATA_ALIGN_BYTES`], and an empty root
/// directory. `serial` is the volume ID, which Linux reads as the
/// filesystem's UUID.
pub fn format<D: Device>(
    dev: &mut D,
    bytes: u64,
    label: &[u8; 11],
    serial: u32,
) -> Result<(), Error> {
    let sector = dev.block_size() as usize;
    if !matches!(sector, 512 | 1024 | 2048 | 4096) {
        return Err(Error::SectorTooSmall);
    }
    let total = u32::try_from(bytes / sector as u64).map_err(|_| Error::TooLarge)?;
    let fats = FATS;
    let align = (DATA_ALIGN_BYTES / sector as u64) as u32;
    let mut reserved = MIN_RESERVED;
    let (mut spc, mut fat_sectors) = fat_layout(total, reserved, sector)?.ok_or(Error::TooLarge)?;
    // A longer reserved region shrinks the FAT, never grows it, so this only
    // climbs, to where the data region's start stops moving.
    loop {
        let aligned = (reserved + fats * fat_sectors).next_multiple_of(align) - fats * fat_sectors;
        if aligned == reserved || aligned > MAX_RESERVED {
            break;
        }
        match fat_layout(total, aligned, sector)? {
            Some(layout) => {
                reserved = aligned;
                (spc, fat_sectors) = layout;
            }
            None => break,
        }
    }

    let mut boot = vec![0u8; sector];
    boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"SLOPOS  ");
    put16(&mut boot, 11, sector as u32);
    boot[13] = spc as u8;
    put16(&mut boot, 14, reserved);
    boot[16] = fats as u8;
    boot[21] = 0xF8;
    put16(&mut boot, 24, 63);
    put16(&mut boot, 26, 255);
    put32(&mut boot, 32, total);
    put32(&mut boot, 36, fat_sectors);
    put32(&mut boot, 44, 2);
    put16(&mut boot, 48, 1);
    put16(&mut boot, 50, 6);
    boot[64] = 0x80;
    boot[66] = 0x29;
    put32(&mut boot, 67, serial);
    boot[71..82].copy_from_slice(label);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;

    let mut info = vec![0u8; sector];
    put32(&mut info, 0, FSINFO_LEAD);
    put32(&mut info, 484, FSINFO_STRUCT);
    let clusters = (total - reserved - fats * fat_sectors) / spc;
    put32(&mut info, 488, clusters - 1);
    put32(&mut info, 492, 3);
    put32(&mut info, 508, FSINFO_TRAIL);

    let at = |sectors: u32| u64::from(sectors) * sector as u64;
    zero_fill(dev, 0, at(reserved + fats * fat_sectors))?;
    dev.write_at(at(6), &boot)?;
    for copy in [0u32, 6] {
        dev.write_at(at(copy + 1), &info)?;
    }
    let mut first_fat = vec![0u8; sector];
    put32(&mut first_fat, 0, 0x0FFF_FFF8);
    put32(&mut first_fat, 4, END_OF_CHAIN);
    put32(&mut first_fat, 8, END_OF_CHAIN);
    for copy in 0..fats {
        dev.write_at(at(reserved + copy * fat_sectors), &first_fat)?;
    }
    let root = u64::from(reserved + fats * fat_sectors) * sector as u64;
    let mut cluster = vec![0u8; spc as usize * sector];
    // The boot sector's label is only believed alongside this entry.
    write_short(&mut cluster[..ENTRY], label, 0, ATTR_VOLUME_ID, 0, 0);
    dev.write_at(root, &cluster)?;
    // The boot sector last, behind the rest: until it lands the partition
    // starts with no volume, which an install cut short reformats.
    dev.flush()?;
    dev.write_at(0, &boot)?;
    dev.flush()
}

/// Largest single transfer `extents` yields: a caller's I/O is one request
/// per extent, and an unbounded one would be a bounce buffer as large as the
/// file.
const MAX_EXTENT: usize = 1 << 20;

/// The first `len` bytes of `chain` as `(first cluster, bytes)` runs of
/// consecutive clusters, each at most [`MAX_EXTENT`]: a file written to a
/// fresh volume is one run, which is one device request per MiB rather than
/// one per cluster.
fn extents(chain: &[u32], cb: usize, len: usize) -> impl Iterator<Item = (u32, usize)> + '_ {
    let per_extent = (MAX_EXTENT / cb).max(1);
    let mut i = 0;
    let mut left = len;
    core::iter::from_fn(move || {
        if left == 0 || i >= chain.len() {
            return None;
        }
        let first = chain[i];
        let mut n = 1;
        while n < per_extent && i + n < chain.len() && chain[i + n] == first + n as u32 {
            n += 1;
        }
        i += n;
        let bytes = (n * cb).min(left);
        left -= bytes;
        Some((first, bytes))
    })
}

#[cfg(test)]
mod tests;
