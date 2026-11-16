//! An image file, its blocks hashed with the SHA instructions where the CPU
//! has them, a long run's on every core.

use std::borrow::Borrow;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;

use slopos_extension_core::image::Source;
use slopos_verity_core::sha256::{self, Compress};

pub struct Disk<F = File>(pub F);

fn ni(state: &mut [u32; 8], blocks: &[u8]) {
    // SAFETY: `compress` hands this out only once `ni::present` found the
    // instructions `ni::compress_blocks` is built with.
    unsafe { sha256::ni::compress_blocks(state, blocks) }
}

/// The fastest way this CPU has to hash.
pub fn compress() -> Compress {
    static CHOSEN: OnceLock<Compress> = OnceLock::new();
    *CHOSEN.get_or_init(|| {
        if sha256::ni::present() {
            ni
        } else {
            sha256::compress_blocks
        }
    })
}

/// The cores an image's blocks are hashed on.
fn cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

impl<F: Borrow<File>> Source for Disk<F> {
    type Error = io::Error;

    fn len(&mut self) -> io::Result<u64> {
        self.0.borrow().metadata().map(|meta| meta.len())
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.0.borrow().read_exact_at(out, offset)
    }

    fn compress(&self) -> Compress {
        compress()
    }

    fn cores(&self) -> usize {
        cores()
    }
}
