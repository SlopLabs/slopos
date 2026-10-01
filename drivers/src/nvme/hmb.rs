//! Host memory lent to a controller without DRAM of its own, which caches its
//! mapping tables there. Granted at probe, taken back before shutdown; while
//! the controller holds it the kernel never touches it.

use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_nvme_core::command::Command;
use slopos_nvme_core::hmb::{self, DESCRIPTOR_BYTES, Grant, HostMemory};
use slopos_nvme_core::identify::HostMemoryRequest;
use slopos_nvme_core::regs::PAGE_SIZE;
use slopos_ostd::mm::dma::DmaCoherent;
use slopos_ostd::{KVec, klog_info};

use super::admin::{AdminError, AdminQueue};

/// The largest chunk asked of the buddy allocator in one piece.
const LARGEST_CHUNK: u64 = 4 << 20;
/// More than this and the drive is caching tables it will rarely touch.
const MOST_GRANTED: u64 = 128 << 20;

#[derive(Default)]
struct Chunks {
    chunks: KVec<DmaCoherent>,
}

impl HostMemory for Chunks {
    fn alloc_chunk(&mut self, bytes: u64) -> bool {
        if self.chunks.try_reserve(1).is_err() {
            return false;
        }
        match DmaCoherent::alloc(bytes as usize / PAGE_SIZE) {
            Ok(chunk) => self.chunks.push(chunk).is_ok(),
            Err(_) => false,
        }
    }

    fn release_all(&mut self) {
        self.chunks.clear();
    }
}

/// A granted buffer: the chunks and the descriptor list naming them.
pub struct HostMemoryBuffer {
    _chunks: KVec<DmaCoherent>,
    _list: OwnedPageFrame,
    bytes: u64,
}

impl HostMemoryBuffer {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Allocate what the controller asks for, within [`MOST_GRANTED`], and
    /// hand it over. `None` declines, which the controller must survive.
    #[inline(never)]
    pub fn grant(admin: &AdminQueue, request: HostMemoryRequest) -> Option<Self> {
        if request.preferred == 0 {
            return None;
        }
        let request = HostMemoryRequest {
            preferred: request.preferred.min(MOST_GRANTED.max(request.minimum)),
            ..request
        };
        let mut memory = Chunks::default();
        let Some(Grant {
            chunk_bytes,
            chunks,
        }) = hmb::grant(&request, LARGEST_CHUNK, &mut memory)
        else {
            klog_info!(
                "nvme: declining the host memory buffer: no {} KiB of chunks to lend",
                request.minimum / 1024
            );
            return None;
        };
        let list = OwnedPageFrame::alloc_zeroed()?;
        for (i, chunk) in memory.chunks.iter().enumerate() {
            let pages = (chunk.len_bytes() / PAGE_SIZE) as u32;
            if !list.write_at(i * DESCRIPTOR_BYTES, &hmb::descriptor(chunk.iova(), pages)) {
                return None;
            }
        }
        let bytes = chunk_bytes * u64::from(chunks);
        let cmd =
            Command::enable_host_memory((bytes / PAGE_SIZE as u64) as u32, list.phys_u64(), chunks);
        match admin.run(cmd) {
            Ok(_) => {}
            // Unanswered, the controller may be using it: it stays lent.
            Err(AdminError::Unanswered) => {
                klog_info!("nvme: no answer to the host memory buffer grant; keeping it lent");
            }
            Err(e) => {
                klog_info!("nvme: host memory buffer not lent: {:?}", e);
                return None;
            }
        }
        Some(Self {
            _chunks: memory.chunks,
            _list: list,
            bytes,
        })
    }

    /// Take the buffer back. The memory stays lent until the controller
    /// acknowledges, so on failure it is kept rather than freed.
    pub fn reclaim(self, admin: &AdminQueue) -> Result<(), Self> {
        match admin.run_polled(Command::disable_host_memory()) {
            Ok(_) => Ok(()),
            Err(_) => Err(self),
        }
    }
}
