//! Namespace churn against the medium: links, unlinks, renames and creates in
//! a few directories, with writeback passes interleaved a step at a time and
//! clean cache entries dropped, checked after every full sync against a mount
//! that reads nothing but what reached the device.
//!
//! This is the shape of a build's object directory — names created, linked
//! into another directory and removed, in whatever order the build finishes
//! — and the class of damage it guards against is a name the medium keeps
//! after its removal, or loses without one.

use slopos_ostd::{KBox, KVec};
use slopos_testing::{TestResult, fail};

use super::journal::journal_image;
use crate::blockdev::MemoryBlockDevice;
use crate::ext2::cache::{BlockCache, CACHE_ENTRIES_MIN};
use crate::ext2::{Ext2Fs, SyncPass};

const DIRS: usize = 3;
const MAX_FILES: usize = 12;
const MAX_NAMES: usize = 160;
const STEPS: u32 = 1500;
const CHECK_EVERY: u32 = 50;
const NAME_MAX: usize = 120;

#[derive(Clone, Copy)]
struct Name {
    dir: usize,
    id: u32,
    ino: u32,
}

struct Model {
    dirs: [u32; DIRS],
    files: KVec<u32>,
    names: KVec<Name>,
    next_id: u32,
    rng: u64,
}

impl Model {
    fn rand(&mut self, bound: usize) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % bound.max(1) as u64) as usize
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn links_of(&self, ino: u32) -> u16 {
        self.names.iter().filter(|n| n.ino == ino).count() as u16
    }
}

/// `n<id>` padded to a length the id picks, so records of every size share
/// blocks and a free record can be reused by a shorter name.
fn name_of(id: u32, out: &mut [u8; NAME_MAX]) -> usize {
    let len = 8 + (id as usize * 37) % (NAME_MAX - 8);
    out[..len].fill(b'x');
    out[0] = b'n';
    let mut v = id;
    for slot in out[1..8].iter_mut().rev() {
        *slot = b'0' + (v % 10) as u8;
        v /= 10;
    }
    len
}

pub fn test_ext2_namespace_churn_matches_the_medium() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    match churn(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

/// Mount `device` and hand the handle to `body`, in a frame of its own: an
/// `Ext2Fs` and the body's locals together outgrow the 2 KiB stack gate.
#[inline(never)]
fn mounted(
    device: &MemoryBlockDevice,
    body: &mut dyn FnMut(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).map_err(|_| "mount_params")?;
    let mut cache = BlockCache::new_boxed(bs, CACHE_ENTRIES_MIN).map_err(|_| "cache")?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).map_err(|_| "mount")?;
    body(&mut fs)
}

#[inline(never)]
fn churn(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    let mut model = KBox::try_new(Model {
        dirs: [0; DIRS],
        files: KVec::new(),
        names: KVec::new(),
        next_id: 0,
        rng: 0x9E37_79B9_7F4A_7C15,
    })
    .map_err(|_| "model")?;
    mounted(device, &mut |fs| churn_on(fs, device, &mut model))
}

#[inline(never)]
fn churn_on(
    fs: &mut Ext2Fs<'_>,
    device: &MemoryBlockDevice,
    model: &mut Model,
) -> Result<(), &'static str> {
    if !matches!(fs.attach_journal(), Ok(Some(_))) {
        return Err("the fixture's log did not attach");
    }
    for d in 0..DIRS {
        let name = [b'd', b'0' + d as u8];
        model.dirs[d] = fs.create_directory(2, &name).map_err(|_| "mkdir")?;
    }
    let mut pass: Option<SyncPass> = None;
    for step in 1..=STEPS {
        churn_step(fs, model, &mut pass)?;
        if step % CHECK_EVERY == 0 {
            if let Some(mut open) = pass.take() {
                while !open.is_done() {
                    fs.sync_step(&mut open, usize::MAX)
                        .map_err(|_| "finishing a pass")?;
                }
            }
            fs.sync().map_err(|_| "sync")?;
            mounted(device, &mut |fresh| check_medium(fresh, model))?;
        }
    }
    Ok(())
}

#[inline(never)]
fn churn_step(
    fs: &mut Ext2Fs<'_>,
    model: &mut Model,
    pass: &mut Option<SyncPass>,
) -> Result<(), &'static str> {
    let mut buf = [0u8; NAME_MAX];
    match model.rand(100) {
        0..35 if !model.files.is_empty() && model.names.len() < MAX_NAMES => {
            let pick = model.rand(model.files.len());
            let ino = model.files[pick];
            let dir = model.rand(DIRS);
            let id = model.fresh_id();
            let len = name_of(id, &mut buf);
            fs.link_entry(model.dirs[dir], &buf[..len], ino)
                .map_err(|_| "link")?;
            model
                .names
                .push(Name { dir, id, ino })
                .map_err(|_| "model")?;
        }
        35..65 if !model.names.is_empty() => {
            let at = model.rand(model.names.len());
            let victim = model.names[at];
            let len = name_of(victim.id, &mut buf);
            fs.unlink_entry(model.dirs[victim.dir], &buf[..len])
                .map_err(|_| "unlink")?;
            model.names.swap_remove(at);
            if model.links_of(victim.ino) == 0
                && let Some(f) = model.files.iter().position(|&i| i == victim.ino)
            {
                model.files.swap_remove(f);
            }
        }
        65..80 if !model.names.is_empty() => rename_step(fs, model)?,
        80..88 if model.files.len() < MAX_FILES && model.names.len() < MAX_NAMES => {
            let dir = model.rand(DIRS);
            let id = model.fresh_id();
            let len = name_of(id, &mut buf);
            let ino = fs
                .create_file(model.dirs[dir], &buf[..len])
                .map_err(|_| "create")?;
            model.files.push(ino).map_err(|_| "model")?;
            model
                .names
                .push(Name { dir, id, ino })
                .map_err(|_| "model")?;
        }
        88..96 => {
            let budget = 1 + model.rand(4);
            let mut open = pass.take().unwrap_or_else(|| fs.begin_sync());
            fs.sync_step(&mut open, budget)
                .map_err(|_| "writeback step")?;
            if !open.is_done() {
                *pass = Some(open);
            }
        }
        96..100 => {
            fs.cache_drop_clean_for_test();
        }
        _ => {}
    }
    Ok(())
}

/// Move a name to a fresh one in a random directory, or over an existing name
/// of another inode, which frees that inode when it was its last link.
#[inline(never)]
fn rename_step(fs: &mut Ext2Fs<'_>, model: &mut Model) -> Result<(), &'static str> {
    let mut old = [0u8; NAME_MAX];
    let mut new = [0u8; NAME_MAX];
    let at = model.rand(model.names.len());
    let from = model.names[at];
    let old_len = name_of(from.id, &mut old);
    let over = model.rand(4) == 0;
    let target = if over {
        let pick = model.rand(model.names.len());
        let t = model.names[pick];
        (t.ino != from.ino).then_some(pick)
    } else {
        None
    };
    let (dir, id) = match target {
        Some(pick) => (model.names[pick].dir, model.names[pick].id),
        None => (model.rand(DIRS), model.fresh_id()),
    };
    let new_len = name_of(id, &mut new);
    fs.rename_entry(
        model.dirs[from.dir],
        &old[..old_len],
        model.dirs[dir],
        &new[..new_len],
    )
    .map_err(|_| "rename")?;
    model.names[at] = Name {
        dir,
        id,
        ino: from.ino,
    };
    if let Some(pick) = target {
        let displaced = model.names[pick].ino;
        model.names.swap_remove(pick);
        if model.links_of(displaced) == 0
            && let Some(f) = model.files.iter().position(|&i| i == displaced)
        {
            model.files.swap_remove(f);
        }
    }
    Ok(())
}

/// Read every name back through a mount that has cached nothing.
#[inline(never)]
fn check_medium(fs: &mut Ext2Fs<'_>, model: &Model) -> Result<(), &'static str> {
    let mut buf = [0u8; NAME_MAX];
    for name in model.names.iter() {
        let len = name_of(name.id, &mut buf);
        let mut found = None;
        let _ = fs.for_each_dir_entry(model.dirs[name.dir], |entry| {
            if entry.name == &buf[..len] {
                found = Some(entry.inode.raw());
                false
            } else {
                true
            }
        });
        if found != Some(name.ino) {
            return Err("a name the model holds is missing or wrong on the medium");
        }
    }
    for (d, &dir) in model.dirs.iter().enumerate() {
        let mut count = 0usize;
        fs.for_each_dir_entry(dir, |_| {
            count += 1;
            true
        })
        .map_err(|_| "a directory would not scan on the medium")?;
        let expected = model.names.iter().filter(|n| n.dir == d).count() + 2;
        if count != expected {
            return Err("the medium holds a name the model removed");
        }
    }
    for &ino in model.files.iter() {
        let links = fs.read_inode(ino).map_err(|_| "read inode")?.links_count;
        if links != model.links_of(ino) {
            return Err("an inode's link count disagrees with the names that reach it");
        }
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_namespace_churn_matches_the_medium,
    suite = fs
);
