//! Free-chunk bins.
//!
//! Exact 16-byte classes up to 1040 bytes, then four bins per power of two,
//! so a free and an allocation cost a bitmap search and a short list walk
//! whatever the arena holds. Large bins are unsorted: a fit takes the first
//! chunk big enough within a bounded walk of its own bin, and the next
//! non-empty bin up — every chunk of which is bigger — otherwise.

use core::ptr;

use super::chunk::{self, ChunkPtr, MIN_CHUNK_SIZE};

pub const SMALL_BIN_COUNT: usize = 64;
/// Four per power of two from 2^10 to 2^32, the largest a chunk header holds.
pub const LARGE_BIN_COUNT: usize = 4 * (32 - 10);
pub const BIN_COUNT: usize = SMALL_BIN_COUNT + LARGE_BIN_COUNT;

const SMALL_BIN_MAX_SIZE: usize = MIN_CHUNK_SIZE + ((SMALL_BIN_COUNT - 1) * chunk::ALIGNMENT);

/// Chunks a fit looks at in its own large bin before it settles for the next
/// bin up, which bounds the walk however many chunks one bin collects.
const FIT_SCAN_LIMIT: usize = 32;

const BINMAP_WORDS: usize = BIN_COUNT.div_ceil(64);

#[derive(Clone, Copy)]
struct Bin {
    head: ChunkPtr,
}

impl Bin {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
        }
    }
}

pub struct BinArray {
    bins: [Bin; BIN_COUNT],
    unsorted: Bin,
    binmap: [u64; BINMAP_WORDS],
}

impl BinArray {
    pub const fn new() -> Self {
        Self {
            bins: [Bin::new(); BIN_COUNT],
            unsorted: Bin::new(),
            binmap: [0; BINMAP_WORDS],
        }
    }

    #[inline]
    pub fn is_empty(&self, bin_idx: usize) -> bool {
        self.bins[bin_idx].head.is_null()
    }

    #[inline]
    pub fn unsorted_is_empty(&self) -> bool {
        self.unsorted.head.is_null()
    }

    #[inline]
    fn mark(&mut self, bin_idx: usize, nonempty: bool) {
        let bit = 1u64 << (bin_idx % 64);
        if nonempty {
            self.binmap[bin_idx / 64] |= bit;
        } else {
            self.binmap[bin_idx / 64] &= !bit;
        }
    }

    #[inline]
    pub fn first_nonempty_from(&self, start: usize) -> Option<usize> {
        let mut word = start / 64;
        if word >= BINMAP_WORDS {
            return None;
        }
        let mut mask = self.binmap[word] & (!0u64 << (start % 64));
        loop {
            if mask != 0 {
                let idx = word * 64 + mask.trailing_zeros() as usize;
                return (idx < BIN_COUNT).then_some(idx);
            }
            word += 1;
            if word >= BINMAP_WORDS {
                return None;
            }
            mask = self.binmap[word];
        }
    }

    pub unsafe fn insert(&mut self, bin_idx: usize, chunk_ptr: ChunkPtr) {
        unsafe {
            Self::insert_front(&mut self.bins[bin_idx].head, chunk_ptr);
        }
        self.mark(bin_idx, true);
    }

    pub unsafe fn insert_unsorted(&mut self, chunk_ptr: ChunkPtr) {
        unsafe {
            Self::insert_front(&mut self.unsorted.head, chunk_ptr);
        }
    }

    pub unsafe fn pop_front(&mut self, bin_idx: usize) -> ChunkPtr {
        let head = self.bins[bin_idx].head;
        if head.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            self.remove(head);
        }
        head
    }

    pub unsafe fn pop_unsorted_front(&mut self) -> ChunkPtr {
        let head = self.unsorted.head;
        if head.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            self.remove(head);
        }
        head
    }

    /// The first chunk of `bin_idx` at least `request_size` long, within
    /// [`FIT_SCAN_LIMIT`] chunks.
    pub unsafe fn find_best_fit(&self, bin_idx: usize, request_size: usize) -> ChunkPtr {
        let head = self.bins[bin_idx].head;
        if head.is_null() {
            return ptr::null_mut();
        }

        let mut current = head;
        for _ in 0..FIT_SCAN_LIMIT {
            if unsafe { chunk::size(current) } >= request_size {
                return current;
            }

            current = unsafe { chunk::fd(current) };
            if current == head {
                break;
            }
        }

        ptr::null_mut()
    }

    /// Unlink a binned chunk. Its size names the only bin it can head, so no
    /// search of the heads is needed; the unsorted list holds any size.
    pub unsafe fn remove(&mut self, chunk_ptr: ChunkPtr) {
        if chunk_ptr.is_null() {
            return;
        }

        let next = unsafe { chunk::fd(chunk_ptr) };
        let prev = unsafe { chunk::bk(chunk_ptr) };
        if next.is_null() || prev.is_null() {
            return;
        }

        let bin_idx = size_to_bin(unsafe { chunk::size(chunk_ptr) });
        let regular_head = bin_idx < BIN_COUNT && self.bins[bin_idx].head == chunk_ptr;
        let unsorted_head = self.unsorted.head == chunk_ptr;

        if next == chunk_ptr && prev == chunk_ptr {
            if regular_head {
                self.bins[bin_idx].head = ptr::null_mut();
                self.mark(bin_idx, false);
            } else if unsorted_head {
                self.unsorted.head = ptr::null_mut();
            }
        } else {
            unsafe {
                chunk::set_fd(prev, next);
                chunk::set_bk(next, prev);
            }

            if regular_head {
                self.bins[bin_idx].head = next;
            } else if unsorted_head {
                self.unsorted.head = next;
            }
        }

        unsafe {
            chunk::clear_links(chunk_ptr);
        }
    }

    unsafe fn insert_front(head: &mut ChunkPtr, chunk_ptr: ChunkPtr) {
        if head.is_null() {
            unsafe {
                chunk::set_fd(chunk_ptr, chunk_ptr);
                chunk::set_bk(chunk_ptr, chunk_ptr);
            }
            *head = chunk_ptr;
            return;
        }

        let position = *head;
        let prev = unsafe { chunk::bk(position) };
        unsafe {
            chunk::set_fd(chunk_ptr, position);
            chunk::set_bk(chunk_ptr, prev);
            chunk::set_fd(prev, chunk_ptr);
            chunk::set_bk(position, chunk_ptr);
        }
        *head = chunk_ptr;
    }

    /// Size of the largest chunk across all bins and the unsorted list.
    pub unsafe fn largest_chunk_size(&self) -> usize {
        let mut largest = 0;
        let heads = self
            .bins
            .iter()
            .map(|bin| bin.head)
            .chain(core::iter::once(self.unsorted.head));
        for head in heads {
            if head.is_null() {
                continue;
            }
            let mut current = head;
            loop {
                largest = largest.max(unsafe { chunk::size(current) });
                current = unsafe { chunk::fd(current) };
                if current == head {
                    break;
                }
            }
        }
        largest
    }
}

impl Default for BinArray {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub fn size_to_small_bin(size: usize) -> Option<usize> {
    if !(MIN_CHUNK_SIZE..=SMALL_BIN_MAX_SIZE).contains(&size) {
        return None;
    }

    if size & (chunk::ALIGNMENT - 1) != 0 {
        return None;
    }

    Some((size - MIN_CHUNK_SIZE) / chunk::ALIGNMENT)
}

/// The large bin a chunk of `size` belongs to; the first large bin for a
/// size the small bins cover.
#[inline]
pub const fn size_to_large_bin(size: usize) -> usize {
    if size <= SMALL_BIN_MAX_SIZE {
        return SMALL_BIN_COUNT;
    }
    let lg = (usize::BITS - 1 - size.leading_zeros()) as usize;
    let sub = (size >> (lg - 2)) & 3;
    let idx = SMALL_BIN_COUNT + (lg - 10) * 4 + sub;
    if idx < BIN_COUNT { idx } else { BIN_COUNT - 1 }
}

#[inline]
pub fn size_to_bin(size: usize) -> usize {
    size_to_small_bin(size).unwrap_or_else(|| size_to_large_bin(size))
}
