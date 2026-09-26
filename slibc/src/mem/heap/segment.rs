//! Segments: `SEGMENT_SIZE`-aligned mappings whose first slice holds the
//! metadata, so any pointer finds its span by masking. A normal segment
//! hands out runs of slices ("spans") under the arena lock; a huge segment
//! carries one object. Mappings the heap lets go of wait in a small cache
//! before they are unmapped: `munmap` is the costliest call the allocator
//! makes (a TLB shootdown across every CPU the process ran on).

use core::ptr::{self, null_mut};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize};

use super::{Heap, OS_PAGE, Racy, SEGMENT_SIZE, SLICE_SHIFT, SLICE_SIZE, SLICES, os};

const SEGMENT_MASK: usize = SEGMENT_SIZE - 1;
const KIND_NORMAL: u8 = 0;
const KIND_HUGE: u8 = 1;
/// Every slice but the metadata one.
const ALL_SLICES: u64 = !1;
const COOKIE_KEY: usize = 0x5105_a110_c8ed_5e61;

const CACHE_ENTRIES: usize = 16;
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESERVE_SEGMENTS: usize = 16;

#[repr(C)]
pub struct Block {
    pub next: *mut Block,
}

/// One span's descriptor. The owning heap's thread alone touches the plain
/// fields of a class span; other threads read `heap`, `block_size`, `start`
/// and `has_aligned` of a span they hold a live block of, which pins them.
#[repr(C)]
pub struct Page {
    pub free: *mut Block,
    pub local_free: *mut Block,
    pub used: u32,
    pub capacity: u32,
    pub reserved: u32,
    pub slice_count: u32,
    /// 0 for a large span or a huge object, which the arena owns.
    pub class: u8,
    pub in_full: bool,
    /// `free` holds only blocks carved from zero memory, whose link word is
    /// their only non-zero byte.
    pub free_fresh: bool,
    /// The span has never been handed out since it was mapped.
    pub mem_fresh: bool,
    pub has_aligned: AtomicBool,
    pub block_size: usize,
    pub start: *mut u8,
    pub heap: AtomicPtr<Heap>,
    /// Blocks other threads freed, with the `FULL`/`NOTIFIED` flags in the
    /// low bits.
    pub xthread: AtomicUsize,
    pub next: *mut Page,
    pub prev: *mut Page,
    pub notify_next: *mut Page,
}

#[repr(C)]
pub struct Segment {
    cookie: usize,
    kind: u8,
    in_list: bool,
    mapped_len: usize,
    /// Bit `i` set: slice `i` is free.
    free_mask: u64,
    /// Bit `i` set: slice `i` has never been handed out, so it is zero.
    fresh_mask: u64,
    used_slices: u32,
    next: *mut Segment,
    prev: *mut Segment,
    /// Each slice's span head. One past the end so a huge object that starts
    /// a whole segment past its header still indexes it.
    span_head: [u8; SLICES + 1],
    pages: [Page; SLICES],
}

const _: () = assert!(size_of::<Segment>() <= SLICE_SIZE);
const _: () = assert!(SLICES == 64);

#[derive(Clone, Copy)]
struct Mapping {
    base: usize,
    len: usize,
}

struct Arena {
    nonfull: *mut Segment,
    /// Aligned, never-touched address space segments are carved from, so a
    /// growing heap pays one `mmap` and two trims per batch of segments
    /// rather than per segment.
    reserve: usize,
    reserve_end: usize,
    segment_bytes: usize,
    huge_count: usize,
    cache: [Mapping; CACHE_ENTRIES],
    cache_len: usize,
    cache_bytes: usize,
}

static ARENA_LOCK: AtomicI32 = AtomicI32::new(0);
static ARENA: Racy<Arena> = Racy::new(Arena {
    nonfull: null_mut(),
    reserve: 0,
    reserve_end: 0,
    segment_bytes: 0,
    huge_count: 0,
    cache: [Mapping { base: 0, len: 0 }; CACHE_ENTRIES],
    cache_len: 0,
    cache_bytes: 0,
});

struct ArenaGuard;

impl ArenaGuard {
    fn lock() -> Self {
        os::lock(&ARENA_LOCK);
        ArenaGuard
    }

    #[allow(clippy::mut_from_ref)]
    fn arena(&self) -> &mut Arena {
        // SAFETY: the guard holds `ARENA_LOCK`, which is what serialises
        // every access to `ARENA`.
        unsafe { &mut *ARENA.get() }
    }
}

impl Drop for ArenaGuard {
    fn drop(&mut self) {
        os::unlock(&ARENA_LOCK);
    }
}

/// Mappings to release once the arena lock is dropped.
struct Unmaps {
    list: [Mapping; CACHE_ENTRIES + 4],
    len: usize,
}

impl Unmaps {
    const fn new() -> Self {
        Unmaps {
            list: [Mapping { base: 0, len: 0 }; CACHE_ENTRIES + 4],
            len: 0,
        }
    }

    fn push(&mut self, m: Mapping) {
        self.list[self.len] = m;
        self.len += 1;
    }

    fn run(self) {
        for m in &self.list[..self.len] {
            // SAFETY: the mapping left the arena's books under its lock, and
            // nothing points into it any more.
            unsafe { os::unmap(m.base as *mut u8, m.len) };
        }
    }
}

pub(super) fn lock_for_fork() {
    os::lock(&ARENA_LOCK);
}

pub(super) fn unlock_after_fork() {
    os::unlock(&ARENA_LOCK);
}

#[inline]
fn cookie_of(seg: *const Segment) -> usize {
    seg as usize ^ COOKIE_KEY
}

/// The segment `p` belongs to. `p - 1` rather than `p`: a huge object with a
/// segment-sized alignment starts exactly one segment past its header.
#[inline]
pub fn segment_of(p: *const u8) -> *mut Segment {
    ((p as usize).wrapping_sub(1) & !SEGMENT_MASK) as *mut Segment
}

/// The span descriptor of `p`, after checking the segment is one of ours.
///
/// # Safety
/// `p` must be a pointer this allocator handed out and not yet freed.
#[inline]
pub unsafe fn page_of(seg: *mut Segment, p: *const u8) -> *mut Page {
    if (*seg).cookie != cookie_of(seg) {
        os::fatal("free(): invalid pointer\n");
    }
    let slice = (p as usize - seg as usize) >> SLICE_SHIFT;
    let head = (*seg).span_head[slice] as usize;
    &raw mut (*seg).pages[head]
}

/// # Safety
/// `page` must be a live span descriptor.
#[inline]
pub unsafe fn is_huge(page: *mut Page) -> bool {
    (*segment_of_page(page)).kind == KIND_HUGE
}

#[inline]
fn segment_of_page(page: *mut Page) -> *mut Segment {
    (page as usize & !SEGMENT_MASK) as *mut Segment
}

#[inline]
fn run_mask(first: usize, count: usize) -> u64 {
    (u64::MAX >> (64 - count)) << first
}

/// The lowest slice starting `count` free slices in `mask`.
fn find_run(mask: u64, count: usize) -> Option<usize> {
    let mut starts = mask;
    let mut have = 1;
    while have < count && starts != 0 {
        let shift = have.min(count - have);
        starts &= starts >> shift;
        have += shift;
    }
    if starts == 0 {
        None
    } else {
        Some(starts.trailing_zeros() as usize)
    }
}

fn longest_run(mask: u64) -> usize {
    let mut m = mask;
    let mut len = 0;
    while m != 0 {
        m &= m >> 1;
        len += 1;
    }
    len
}

impl Arena {
    unsafe fn link(&mut self, seg: *mut Segment) {
        (*seg).prev = null_mut();
        (*seg).next = self.nonfull;
        if !self.nonfull.is_null() {
            (*self.nonfull).prev = seg;
        }
        self.nonfull = seg;
        (*seg).in_list = true;
    }

    unsafe fn unlink(&mut self, seg: *mut Segment) {
        let (prev, next) = ((*seg).prev, (*seg).next);
        if prev.is_null() {
            self.nonfull = next;
        } else {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*seg).in_list = false;
    }

    unsafe fn take_fit(&mut self, count: usize) -> *mut Page {
        let mut seg = self.nonfull;
        while !seg.is_null() {
            if let Some(first) = find_run((*seg).free_mask, count) {
                return self.take(seg, first, count);
            }
            seg = (*seg).next;
        }
        null_mut()
    }

    unsafe fn take(&mut self, seg: *mut Segment, first: usize, count: usize) -> *mut Page {
        let bits = run_mask(first, count);
        (*seg).free_mask &= !bits;
        let fresh = (*seg).fresh_mask & bits == bits;
        (*seg).fresh_mask &= !bits;
        (*seg).used_slices += count as u32;
        for slice in first..first + count {
            (*seg).span_head[slice] = first as u8;
        }
        if (*seg).free_mask == 0 {
            self.unlink(seg);
        }
        let page = &raw mut (*seg).pages[first];
        (*page).slice_count = count as u32;
        (*page).start = (seg as *mut u8).add(first << SLICE_SHIFT);
        (*page).mem_fresh = fresh;
        page
    }

    unsafe fn install(&mut self, m: Mapping, fresh: bool) {
        let seg = m.base as *mut Segment;
        (*seg).cookie = cookie_of(seg);
        (*seg).kind = KIND_NORMAL;
        (*seg).mapped_len = m.len;
        (*seg).free_mask = ALL_SLICES;
        (*seg).fresh_mask = if fresh { ALL_SLICES } else { 0 };
        (*seg).used_slices = 0;
        self.segment_bytes += m.len;
        self.link(seg);
    }

    /// Map a fresh batch of segments: a quarter of what the heap already
    /// holds, so the number of batches grows with the log of the heap.
    fn refill_reserve(&mut self, unmaps: &mut Unmaps) -> bool {
        let batch = (self.segment_bytes / 4 / SEGMENT_SIZE).clamp(1, MAX_RESERVE_SEGMENTS);
        for count in [batch, 1] {
            if let Some(base) = map_aligned(count * SEGMENT_SIZE, SEGMENT_SIZE, unmaps) {
                self.reserve = base;
                self.reserve_end = base + count * SEGMENT_SIZE;
                return true;
            }
        }
        false
    }

    /// The best-fitting cached mapping of at least `need` and at most `max`
    /// bytes.
    fn cache_take(&mut self, need: usize, max: usize) -> Option<Mapping> {
        let mut best: Option<usize> = None;
        for (i, m) in self.cache[..self.cache_len].iter().enumerate() {
            if m.len >= need && m.len <= max && best.is_none_or(|b| m.len < self.cache[b].len) {
                best = Some(i);
            }
        }
        let i = best?;
        let m = self.cache[i];
        self.cache.copy_within(i + 1..self.cache_len, i);
        self.cache_len -= 1;
        self.cache_bytes -= m.len;
        Some(m)
    }

    fn cache_put(&mut self, m: Mapping, unmaps: &mut Unmaps) {
        if m.len > CACHE_MAX_BYTES {
            unmaps.push(m);
            return;
        }
        while self.cache_len == CACHE_ENTRIES || self.cache_bytes + m.len > CACHE_MAX_BYTES {
            let oldest = self.cache[0];
            self.cache.copy_within(1..self.cache_len, 0);
            self.cache_len -= 1;
            self.cache_bytes -= oldest.len;
            unmaps.push(oldest);
        }
        self.cache[self.cache_len] = m;
        self.cache_len += 1;
        self.cache_bytes += m.len;
    }
}

/// Map `len` bytes whose start is `align`-aligned, trimming the slack into
/// `unmaps`.
fn map_aligned(len: usize, align: usize, unmaps: &mut Unmaps) -> Option<usize> {
    let total = len.checked_add(align - OS_PAGE)?;
    let base = os::map(total) as usize;
    if base == 0 {
        return None;
    }
    let start = base.next_multiple_of(align);
    if start > base {
        unmaps.push(Mapping {
            base,
            len: start - base,
        });
    }
    if base + total > start + len {
        unmaps.push(Mapping {
            base: start + len,
            len: base + total - (start + len),
        });
    }
    Some(start)
}

/// Map `payload` bytes preceded by a metadata slice, as `(header, object,
/// mapped length)`: the header is segment-aligned and the object sits past
/// the metadata at `align`, no further than one segment from the header.
fn map_segment(payload: usize, align: usize, unmaps: &mut Unmaps) -> Option<(usize, usize, usize)> {
    let payload = payload.checked_next_multiple_of(OS_PAGE)?;
    let align = align.max(SLICE_SIZE);
    // Room for the object at `align` past the metadata slice, measured from
    // an aligned header.
    let lead = if align <= SLICE_SIZE {
        SLICE_SIZE
    } else {
        align.max(SEGMENT_SIZE)
    };
    let base = map_aligned(lead.checked_add(payload)?, SEGMENT_SIZE, unmaps)?;
    let object = (base + SLICE_SIZE).next_multiple_of(align);
    let header = (object - 1) & !SEGMENT_MASK;
    let end = object + payload;
    if header > base {
        unmaps.push(Mapping {
            base,
            len: header - base,
        });
    }
    if base + lead + payload > end {
        unmaps.push(Mapping {
            base: end,
            len: base + lead + payload - end,
        });
    }
    Some((header, object, end - header))
}

/// A span of `count` slices from some normal segment, its `start`,
/// `slice_count` and `mem_fresh` filled in. Null when memory is exhausted.
pub fn alloc_span(count: usize) -> *mut Page {
    debug_assert!(count >= 1 && count < SLICES);
    let mut unmaps = Unmaps::new();
    let page = {
        let guard = ArenaGuard::lock();
        let arena = guard.arena();
        // SAFETY: under the arena lock every listed segment is live, and a
        // mapping taken from the cache or the reserve is the arena's alone.
        unsafe {
            loop {
                let page = arena.take_fit(count);
                if !page.is_null() {
                    break page;
                }
                if let Some(m) = arena.cache_take(SEGMENT_SIZE, 2 * SEGMENT_SIZE) {
                    arena.install(m, false);
                } else if arena.reserve < arena.reserve_end {
                    let base = arena.reserve;
                    arena.reserve += SEGMENT_SIZE;
                    arena.install(
                        Mapping {
                            base,
                            len: SEGMENT_SIZE,
                        },
                        true,
                    );
                } else if !arena.refill_reserve(&mut unmaps) {
                    break null_mut();
                }
            }
        }
    };
    unmaps.run();
    page
}

/// Return a span to its segment, releasing the segment when it empties.
///
/// # Safety
/// `page` must head a span [`alloc_span`] gave out and nothing may use it.
pub unsafe fn free_span(page: *mut Page) {
    let mut unmaps = Unmaps::new();
    {
        let guard = ArenaGuard::lock();
        let arena = guard.arena();
        let seg = segment_of_page(page);
        let first = ((*page).start as usize - seg as usize) >> SLICE_SHIFT;
        let count = (*page).slice_count as usize;
        let bits = run_mask(first, count);
        if (*seg).free_mask & bits != 0 || (*seg).span_head[first] as usize != first {
            drop(guard);
            os::fatal("free(): double free\n");
        }
        (*seg).free_mask |= bits;
        (*seg).used_slices -= count as u32;
        if (*seg).used_slices == 0 {
            if (*seg).in_list {
                arena.unlink(seg);
            }
            arena.segment_bytes -= (*seg).mapped_len;
            (*seg).cookie = 0;
            let m = Mapping {
                base: seg as usize,
                len: (*seg).mapped_len,
            };
            arena.cache_put(m, &mut unmaps);
        } else if !(*seg).in_list {
            arena.link(seg);
        }
    }
    unmaps.run();
}

/// Extend a large span in place to `count` slices if the slices after it
/// are free.
///
/// # Safety
/// `page` must head a live large span.
pub unsafe fn grow_span(page: *mut Page, count: usize) -> bool {
    let guard = ArenaGuard::lock();
    let arena = guard.arena();
    let seg = segment_of_page(page);
    let first = ((*page).start as usize - seg as usize) >> SLICE_SHIFT;
    let have = (*page).slice_count as usize;
    if first + count > SLICES {
        return false;
    }
    let extra = run_mask(first + have, count - have);
    if (*seg).free_mask & extra != extra {
        return false;
    }
    (*seg).free_mask &= !extra;
    (*seg).fresh_mask &= !extra;
    (*seg).used_slices += (count - have) as u32;
    for slice in first + have..first + count {
        (*seg).span_head[slice] = first as u8;
    }
    if (*seg).free_mask == 0 && (*seg).in_list {
        arena.unlink(seg);
    }
    (*page).slice_count = count as u32;
    (*page).block_size = count << SLICE_SHIFT;
    true
}

/// Give the slices of a large span past `count` back to its segment.
///
/// # Safety
/// `page` must head a live large span of more than `count` slices.
pub unsafe fn shrink_span(page: *mut Page, count: usize) {
    let guard = ArenaGuard::lock();
    let arena = guard.arena();
    let seg = segment_of_page(page);
    let first = ((*page).start as usize - seg as usize) >> SLICE_SHIFT;
    let have = (*page).slice_count as usize;
    (*seg).free_mask |= run_mask(first + count, have - count);
    (*seg).used_slices -= (have - count) as u32;
    if !(*seg).in_list {
        arena.link(seg);
    }
    (*page).slice_count = count as u32;
    (*page).block_size = count << SLICE_SHIFT;
}

/// A huge object of `size` bytes at `align`, in a mapping of its own, and
/// whether its memory is known zero.
pub fn alloc_huge(size: usize, align: usize) -> (*mut u8, bool) {
    let Some(need) = size
        .checked_add(SLICE_SIZE)
        .and_then(|n| n.checked_next_multiple_of(OS_PAGE))
    else {
        return (null_mut(), false);
    };
    if align <= SLICE_SIZE {
        let guard = ArenaGuard::lock();
        let arena = guard.arena();
        if let Some(m) = arena.cache_take(need, need.saturating_add(need / 2)) {
            arena.huge_count += 1;
            drop(guard);
            // SAFETY: the cached mapping is ours alone now.
            let p = unsafe { install_huge(m.base, m.base + SLICE_SIZE, m.len) };
            return (p, false);
        }
    }
    let mut unmaps = Unmaps::new();
    let mapped = map_segment(size, align, &mut unmaps);
    unmaps.run();
    let Some((header, object, len)) = mapped else {
        return (null_mut(), false);
    };
    ArenaGuard::lock().arena().huge_count += 1;
    // SAFETY: the fresh mapping is ours alone.
    (unsafe { install_huge(header, object, len) }, true)
}

unsafe fn install_huge(header: usize, object: usize, len: usize) -> *mut u8 {
    let seg = header as *mut Segment;
    (*seg).cookie = cookie_of(seg);
    (*seg).kind = KIND_HUGE;
    (*seg).in_list = false;
    (*seg).mapped_len = len;
    (*seg).span_head = [0; SLICES + 1];
    let page = &raw mut (*seg).pages[0];
    ptr::write(
        page,
        Page {
            free: null_mut(),
            local_free: null_mut(),
            used: 1,
            capacity: 1,
            reserved: 1,
            slice_count: 0,
            class: 0,
            in_full: false,
            free_fresh: false,
            mem_fresh: false,
            has_aligned: AtomicBool::new(false),
            block_size: header + len - object,
            start: object as *mut u8,
            heap: AtomicPtr::new(null_mut()),
            xthread: AtomicUsize::new(0),
            next: null_mut(),
            prev: null_mut(),
            notify_next: null_mut(),
        },
    );
    object as *mut u8
}

/// # Safety
/// `page` must describe a live huge object nothing will use again.
pub unsafe fn free_huge(page: *mut Page) {
    let seg = segment_of_page(page);
    let m = Mapping {
        base: seg as usize,
        len: (*seg).mapped_len,
    };
    (*seg).cookie = 0;
    let mut unmaps = Unmaps::new();
    {
        let guard = ArenaGuard::lock();
        let arena = guard.arena();
        arena.huge_count -= 1;
        arena.cache_put(m, &mut unmaps);
    }
    unmaps.run();
}

/// `(segment bytes, cached bytes, largest free span, huge objects)`.
pub fn stats() -> (usize, usize, usize, usize) {
    let guard = ArenaGuard::lock();
    let arena = guard.arena();
    let mut largest = 0;
    let mut seg = arena.nonfull;
    while !seg.is_null() {
        // SAFETY: listed segments are live under the lock.
        unsafe {
            largest = largest.max(longest_run((*seg).free_mask));
            seg = (*seg).next;
        }
    }
    (
        arena.segment_bytes,
        arena.cache_bytes + (arena.reserve_end - arena.reserve),
        largest << SLICE_SHIFT,
        arena.huge_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_match_a_bit_by_bit_search() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mask = x & x.rotate_left(7) & ALL_SLICES;
            for count in 1..SLICES {
                let naive =
                    (0..=SLICES - count).find(|&i| (i..i + count).all(|b| mask >> b & 1 == 1));
                assert_eq!(find_run(mask, count), naive, "{mask:#x} {count}");
            }
            let longest = (1..=SLICES)
                .rev()
                .find(|&n| find_run(mask, n).is_some())
                .unwrap_or(0);
            assert_eq!(longest_run(mask), longest);
        }
    }
}
