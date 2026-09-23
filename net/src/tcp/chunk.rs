//! Connection byte streams in page-sized chunks that exist only while they
//! hold bytes, so a connection costs what it has buffered, not its window.
//!
//! A chunk is never allocated under a PCB lock: that lock masks interrupts, and
//! a page allocation can wait on a cross-CPU TLB drain that a peer spinning on
//! the lock never acknowledges. Callers reserve [`Spares`] before taking it.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::mm::{VmReader, VmWriter};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{AllocError, KBox, KVec};

/// A page less the heap's 32-byte header for allocations past 2 KiB, so a
/// chunk costs one page rather than two.
pub const CHUNK_SIZE: usize = 4096 - 32;

/// What one connection may buffer in each direction on the largest machine.
pub const TCP_BUFFER_CEILING: usize = 4 * 1024 * 1024;
/// What one connection may buffer on the smallest.
pub const TCP_BUFFER_FLOOR: usize = 256 * 1024;
/// Linux's per-connection share of memory, `tcp_rmem[2]`.
const BUFFER_MEMORY_SHARE: u64 = 128;
/// All connections together, `tcp_mem`'s order of magnitude.
const TOTAL_MEMORY_SHARE: u64 = 16;
const TOTAL_FLOOR: usize = 8 * 1024 * 1024;

const CACHE_CHUNKS: usize = 256;

/// Chunks every stream may hold past the machine-wide ceiling, so a full pool
/// slows each connection rather than stopping one. Out-of-order-only chunks do
/// not count, or they could take the one the gap-filling segment needs.
pub const RESERVED_CHUNKS: usize = 4;

/// Chunks a ring may fill past its stream for out-of-order bytes; unbounded,
/// one byte per chunk across a wide window pins a chunk per byte.
pub const OUT_OF_ORDER_CHUNKS: usize = 64;

/// The most chunks one reservation takes: two cover any segment, and a write
/// larger than this loops.
pub const SPARES_MAX: usize = 16;

static BUFFER_MAX: AtomicUsize = AtomicUsize::new(TCP_BUFFER_FLOOR);
static CHUNK_CEILING: AtomicUsize = AtomicUsize::new(TOTAL_FLOOR / CHUNK_SIZE);
static LIVE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static CACHED_CHUNKS: AtomicUsize = AtomicUsize::new(0);

static CACHE: SpinLock<KVec<Chunk>> = SpinLock::new(
    KVec::new(),
    lock_class!("TCP_CHUNK_CACHE", LOCK_LEVEL_RESOURCE),
);

/// Size the buffers from memory, once the page allocator is seeded. Returns
/// `(per-connection bytes, total bytes)`.
pub fn install_limits(usable_frames: u32) -> (usize, usize) {
    let usable = usable_frames as u64 * slopos_ostd::mm::page_table::PAGE_SIZE_4KB;
    let (per_conn, total) = derive_limits(usable);
    BUFFER_MAX.store(per_conn, Ordering::Release);
    CHUNK_CEILING.store(total / CHUNK_SIZE, Ordering::Release);
    (per_conn, total)
}

pub fn derive_limits(usable_bytes: u64) -> (usize, usize) {
    let per_conn = (usable_bytes / BUFFER_MEMORY_SHARE)
        .clamp(TCP_BUFFER_FLOOR as u64, TCP_BUFFER_CEILING as u64) as usize;
    let total = ((usable_bytes / TOTAL_MEMORY_SHARE) as usize).max(TOTAL_FLOOR);
    (per_conn, total)
}

/// The most one connection may buffer in either direction, and the default.
pub fn buffer_max() -> usize {
    BUFFER_MAX.load(Ordering::Acquire)
}

pub fn live_chunks() -> usize {
    LIVE_CHUNKS.load(Ordering::Acquire)
}

pub fn cached_chunks() -> usize {
    CACHED_CHUNKS.load(Ordering::Acquire)
}

pub fn chunk_ceiling() -> usize {
    CHUNK_CEILING.load(Ordering::Acquire)
}

/// Bytes the machine-wide ceiling still admits, counting cached chunks as
/// admitted: a window larger than this invites bytes that would be dropped.
pub fn room_bytes() -> usize {
    let unallocated = chunk_ceiling().saturating_sub(live_chunks());
    (unallocated + CACHED_CHUNKS.load(Ordering::Acquire)) * CHUNK_SIZE
}

/// Lets a test reach the machine-wide ceiling without allocating up to it.
#[cfg(feature = "test-hooks")]
pub fn swap_chunk_ceiling(chunks: usize) -> usize {
    CHUNK_CEILING.swap(chunks, Ordering::AcqRel)
}

pub struct Chunk(KBox<[u8; CHUNK_SIZE]>);

impl Chunk {
    fn allocate(past_ceiling: bool) -> Option<Self> {
        let ceiling = chunk_ceiling();
        LIVE_CHUNKS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (past_ceiling || n < ceiling).then_some(n + 1)
            })
            .ok()?;
        match KBox::<[u8; CHUNK_SIZE]>::zeroed() {
            Ok(bytes) => Some(Self(bytes)),
            Err(AllocError) => {
                LIVE_CHUNKS.fetch_sub(1, Ordering::AcqRel);
                None
            }
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        LIVE_CHUNKS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Legal under a PCB lock: pushing never grows the cache.
fn recycle(chunk: Chunk) {
    let mut cache = CACHE.lock();
    if cache.len() < cache.capacity() {
        let _ = cache.push(chunk);
        CACHED_CHUNKS.store(cache.len(), Ordering::Release);
    }
}

fn take_cached() -> Option<Chunk> {
    let mut cache = CACHE.lock();
    let chunk = cache.pop();
    CACHED_CHUNKS.store(cache.len(), Ordering::Release);
    chunk
}

fn ensure_cache_storage() {
    if CACHE.lock().capacity() >= CACHE_CHUNKS {
        return;
    }
    let Ok(mut storage) = KVec::with_capacity(CACHE_CHUNKS) else {
        return;
    };
    let mut cache = CACHE.lock();
    if cache.capacity() < CACHE_CHUNKS {
        while let Some(chunk) = cache.pop() {
            let _ = storage.push(chunk);
        }
        *cache = storage;
    }
}

/// Chunks reserved outside a PCB lock for a ring to draw on inside it.
pub struct Spares {
    chunks: [Option<Chunk>; SPARES_MAX],
    len: usize,
}

impl Default for Spares {
    fn default() -> Self {
        Self::new()
    }
}

impl Spares {
    pub const fn new() -> Self {
        Self {
            chunks: [const { None }; SPARES_MAX],
            len: 0,
        }
    }

    /// Enough chunks for `bytes` wherever they land in a ring holding `held`;
    /// short when the ceiling or the allocator says no.
    pub fn for_bytes(bytes: usize, held: usize) -> Self {
        let mut spares = Self::new();
        if bytes > 0 {
            spares.reserve(bytes.div_ceil(CHUNK_SIZE) + 1, held);
        }
        spares
    }

    /// Up to `want` chunks for a ring that holds `held`; the ring's reserved
    /// share is taken past the ceiling if the ceiling has none to give.
    pub fn reserve(&mut self, want: usize, held: usize) {
        ensure_cache_storage();
        let want = want.min(SPARES_MAX);
        let mut reserved = RESERVED_CHUNKS.saturating_sub(held);
        while self.len < want {
            let chunk = match take_cached().or_else(|| Chunk::allocate(false)) {
                Some(chunk) => chunk,
                None if reserved > 0 => {
                    let Some(chunk) = Chunk::allocate(true) else {
                        break;
                    };
                    reserved -= 1;
                    chunk
                }
                None => break,
            };
            self.chunks[self.len] = Some(chunk);
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn take(&mut self) -> Option<Chunk> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        self.chunks[self.len].take()
    }
}

impl Drop for Spares {
    fn drop(&mut self) {
        while let Some(chunk) = self.take() {
            recycle(chunk);
        }
    }
}

/// A byte stream of up to `capacity` bytes over lazily filled chunks, with
/// room past the end for out-of-order bytes that have not been joined yet.
pub struct ChunkRing {
    slots: KVec<Option<Chunk>>,
    /// Position of the first byte, in the slots' circular byte space.
    head: usize,
    len: usize,
    capacity: usize,
    held: usize,
}

impl ChunkRing {
    /// A ring that may grow to `max` bytes, starting at `capacity`. The slot
    /// table is the only allocation, and it is taken here, outside any lock.
    pub fn new(max: usize, capacity: usize) -> Result<Self, AllocError> {
        let nslots = max.div_ceil(CHUNK_SIZE) + 1;
        let mut slots = KVec::with_capacity(nslots)?;
        for _ in 0..nslots {
            slots.push(None)?;
        }
        Ok(Self {
            slots,
            head: 0,
            len: 0,
            capacity: capacity.min(max),
            held: 0,
        })
    }

    /// One slot is always spare, so an out-of-order write at the edge of the
    /// window never wraps onto the chunk the head is in.
    pub fn max_capacity(&self) -> usize {
        (self.slots.len() - 1) * CHUNK_SIZE
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.min(self.max_capacity());
    }

    pub fn free_space(&self) -> usize {
        self.capacity.saturating_sub(self.len)
    }

    /// Bytes a write takes without a spare: the rest of the chunk the stream
    /// ends in.
    pub fn tail_room(&self) -> usize {
        let (slot, off) = self.locate(self.len);
        match self.slots[slot] {
            Some(_) => CHUNK_SIZE - off,
            None => 0,
        }
    }

    pub fn chunks_held(&self) -> usize {
        self.held
    }

    /// The chunks the stream's own bytes are in, leaving out those that hold
    /// only out-of-order bytes past its end.
    pub fn stream_chunks(&self) -> usize {
        let (_, off) = self.locate(0);
        (off + self.len).div_ceil(CHUNK_SIZE)
    }

    /// Bytes chunks can still be found for: the ring's reserved share, then
    /// what the machine-wide ceiling admits.
    pub fn backed_room(&self) -> usize {
        RESERVED_CHUNKS.saturating_sub(self.held) * CHUNK_SIZE + room_bytes()
    }

    fn span(&self) -> usize {
        self.slots.len() * CHUNK_SIZE
    }

    /// The slot and in-chunk offset of stream position `pos`.
    fn locate(&self, pos: usize) -> (usize, usize) {
        let at = (self.head + pos) % self.span();
        (at / CHUNK_SIZE, at % CHUNK_SIZE)
    }

    fn span_mut(
        &mut self,
        pos: usize,
        spares: &mut Spares,
        fill_budget: &mut usize,
    ) -> Option<&mut [u8]> {
        let (slot, off) = self.locate(pos);
        if self.slots[slot].is_none() {
            *fill_budget = fill_budget.checked_sub(1)?;
            self.slots[slot] = Some(spares.take()?);
            self.held += 1;
        }
        let chunk = self.slots[slot].as_mut()?;
        Some(&mut chunk.0[off..])
    }

    fn span_ref(&self, pos: usize) -> Option<&[u8]> {
        let (slot, off) = self.locate(pos);
        self.slots[slot].as_ref().map(|chunk| &chunk.0[off..])
    }

    fn place(
        &mut self,
        pos: usize,
        data: &[u8],
        limit: usize,
        spares: &mut Spares,
        mut fill_budget: usize,
    ) -> usize {
        let want = data.len().min(limit);
        let mut done = 0;
        while done < want {
            let Some(dst) = self.span_mut(pos + done, spares, &mut fill_budget) else {
                break;
            };
            let n = dst.len().min(want - done);
            dst[..n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        done
    }

    pub fn write(&mut self, data: &[u8], spares: &mut Spares) -> usize {
        let wrote = self.place(self.len, data, self.free_space(), spares, usize::MAX);
        self.len += wrote;
        wrote
    }

    pub fn write_from(
        &mut self,
        reader: &mut VmReader<'_>,
        max: usize,
        spares: &mut Spares,
    ) -> usize {
        let want = self.free_space().min(max).min(reader.remain());
        let mut done = 0;
        let mut fill_budget = usize::MAX;
        while done < want {
            let pos = self.len + done;
            let Some(dst) = self.span_mut(pos, spares, &mut fill_budget) else {
                break;
            };
            let n = dst.len().min(want - done);
            let got = reader.read(&mut dst[..n]);
            done += got;
            if got < n {
                break;
            }
        }
        self.len += done;
        done
    }

    /// Place bytes `offset` past the end without making them part of the
    /// stream; [`advance`](Self::advance) joins them once the gap fills.
    pub fn write_at(&mut self, offset: usize, data: &[u8], spares: &mut Spares) -> usize {
        let room = self.free_space().saturating_sub(offset);
        let stream = self.len.div_ceil(CHUNK_SIZE) + 1;
        let fill_budget = (stream + OUT_OF_ORDER_CHUNKS).saturating_sub(self.held);
        self.place(self.len + offset, data, room, spares, fill_budget)
    }

    /// Join up to `n` bytes already placed by [`write_at`](Self::write_at),
    /// returning how many joined: fewer when the capacity shrank after they
    /// were placed.
    pub fn advance(&mut self, n: usize) -> usize {
        let n = n.min(self.free_space());
        self.len += n;
        n
    }

    pub fn peek_at(&self, offset: usize, out: &mut [u8]) -> usize {
        let want = out.len().min(self.len.saturating_sub(offset));
        let mut done = 0;
        while done < want {
            let Some(src) = self.span_ref(offset + done) else {
                break;
            };
            let n = src.len().min(want - done);
            out[done..done + n].copy_from_slice(&src[..n]);
            done += n;
        }
        done
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = self.peek_at(0, out);
        self.consume(n);
        n
    }

    pub fn read_into(&mut self, writer: &mut VmWriter<'_>) -> usize {
        let want = self.len.min(writer.remain());
        let mut done = 0;
        while done < want {
            let Some(src) = self.span_ref(done) else {
                break;
            };
            let n = src.len().min(want - done);
            let put = writer.write(&src[..n]);
            done += put;
            if put < n {
                break;
            }
        }
        self.consume(done);
        done
    }

    /// The chunk the new head sits in stays even when the stream empties: it
    /// may already hold out-of-order bytes.
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.len);
        let (first, _) = self.locate(0);
        let (last, _) = self.locate(n);
        let mut slot = first;
        while slot != last {
            if let Some(chunk) = self.slots[slot].take() {
                self.held -= 1;
                recycle(chunk);
            }
            slot = (slot + 1) % self.slots.len();
        }
        self.head = (self.head + n) % self.span();
        self.len -= n;
    }

    pub fn reset(&mut self) {
        for slot in self.slots.iter_mut() {
            if let Some(chunk) = slot.take() {
                recycle(chunk);
            }
        }
        self.held = 0;
        self.head = 0;
        self.len = 0;
    }
}

impl Drop for ChunkRing {
    fn drop(&mut self) {
        self.reset();
    }
}
