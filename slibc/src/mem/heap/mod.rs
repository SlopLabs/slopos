//! The allocator: per-thread heaps over size-class spans, in the manner of
//! mimalloc.
//!
//! Memory comes in [`SEGMENT_SIZE`]-aligned segments of 64 KiB slices, so a
//! pointer finds its span's descriptor by masking. A span of a size class
//! belongs to one thread's heap, which allocates from it and frees into it
//! without a lock or an atomic; a thread freeing another heap's block pushes
//! it onto the span's atomic `xthread` list, which the owner collects when
//! the span runs dry. A span with no free block leaves its class queue; the
//! first foreign free into it hands the span to the owner's `notify` list,
//! which the owner drains on its next slow path.
//!
//! Heaps are never freed. A thread's exit returns empty spans to their
//! segments and parks its heap in a pool, where the next new thread adopts
//! it whole — its spans, and the blocks other threads freed into them since.
//!
//! Spans past the size classes up to [`LARGE_MAX`] come straight from the
//! arena (under its lock) and anything larger gets a mapping of its own.
//!
//! Lock order: the registry lock, then heap locks, then the arena lock. A
//! heap's lock is taken only by its own slow paths, by whoever cleans a
//! pooled heap, and by `fork`, which holds all three levels so that the child
//! inherits every heap consistent.

use core::cell::UnsafeCell;
use core::ptr::{self, null_mut};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, Ordering, compiler_fence};

use super::os;

mod class;
mod segment;

use class::{CLASSES, class_of, class_size, span_slices};
use segment::{Block, Page, page_of, segment_of};

pub const MIN_ALIGN: usize = 16;
const SLICE_SHIFT: usize = 16;
const SLICE_SIZE: usize = 1 << SLICE_SHIFT;
pub const SEGMENT_SIZE: usize = 4 * 1024 * 1024;
const SLICES: usize = SEGMENT_SIZE / SLICE_SIZE;
const OS_PAGE: usize = 4096;
/// Largest size-class block.
pub const MAX_CLASS_SIZE: usize = 256 * 1024;
/// Largest object served from a segment span; past it, a mapping of its own.
pub const LARGE_MAX: usize = 2 * 1024 * 1024;
/// How much of a span one refill carves into blocks, so a span is faulted in
/// as it is used rather than all at once. Four pages: the kernel maps an
/// anonymous fault's window ahead anyway, and a smaller carve sends a
/// mid-sized class back to the slow path every block or two.
const EXTEND_BYTES: usize = 16 * 1024;
/// An empty span of at most this many slices stays at the head of its queue.
const KEEP_EMPTY_SLICES: u32 = 4;
/// Slow paths between looks at the pool for foreign frees to collect.
const CLEAN_INTERVAL: u32 = 64;

const FULL: usize = 1;
const NOTIFIED: usize = 2;
const FLAGS: usize = FULL | NOTIFIED;

const OWNED: u8 = 0;
const POOLED: u8 = 1;
const CLEANING: u8 = 2;

struct Racy<T>(UnsafeCell<T>);

// SAFETY: every `Racy` static documents the lock that serialises it.
unsafe impl<T> Sync for Racy<T> {}

impl<T> Racy<T> {
    const fn new(value: T) -> Self {
        Racy(UnsafeCell::new(value))
    }

    const fn get(&self) -> *mut T {
        self.0.get()
    }
}

#[repr(C)]
struct Queue {
    first: *mut Page,
    last: *mut Page,
}

/// One thread's heap. All-zero is an empty heap owned by whoever made it.
#[repr(C)]
pub struct Heap {
    queues: [Queue; CLASSES + 1],
    full: Queue,
    notify: AtomicPtr<Page>,
    lock: AtomicI32,
    state: AtomicU8,
    slow_count: u32,
    /// The last cleaning pass that visited it; guarded by the registry lock.
    clean_pass: u32,
    next_all: *mut Heap,
    next_pool: *mut Heap,
}

const _: () = assert!(size_of::<Heap>() <= HEAP_CHUNK);

/// Heaps are carved from mappings of this size and never returned.
const HEAP_CHUNK: usize = 64 * 1024;

/// Guarded by `REGISTRY_LOCK`.
struct Registry {
    all: *mut Heap,
    pool: *mut Heap,
    spare: *mut u8,
    spare_left: usize,
    clean_pass: u32,
}

static REGISTRY_LOCK: AtomicI32 = AtomicI32::new(0);
static REGISTRY: Racy<Registry> = Racy::new(Registry {
    all: null_mut(),
    pool: null_mut(),
    spare: null_mut(),
    spare_left: 0,
    clean_pass: 0,
});
/// Set by a foreign free into a pooled heap's span: memory worth collecting.
static POOL_DIRTY: AtomicBool = AtomicBool::new(false);

/// What `heap_stats` reports.
#[derive(Clone, Copy, Debug)]
pub struct HeapStats {
    /// Address space in segments that hold live spans.
    pub arena_size: usize,
    /// Address space mapped but holding nothing: released segments and huge
    /// mappings kept for reuse, and segments not yet handed out.
    pub cached_size: usize,
    /// Largest run of free slices in any live segment.
    pub largest_free: usize,
    /// Live objects with a mapping of their own.
    pub direct_count: usize,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `malloc`. Null only when memory is exhausted; `alloc(0)` is a minimal
/// block.
#[inline]
pub fn alloc(size: usize) -> *mut u8 {
    if size <= MAX_CLASS_SIZE {
        // SAFETY: the slot and a non-null heap in it are this thread's own.
        unsafe {
            let heap = *os::heap_slot();
            if !heap.is_null() {
                let page = (*heap).queues[class_of(size)].first;
                if !page.is_null() {
                    let block = (*page).free;
                    if !block.is_null() {
                        (*page).free = (*block).next;
                        (*page).used += 1;
                        return block.cast();
                    }
                }
            }
        }
        return alloc_class_slow(class_of(size)).0;
    }
    alloc_big(size, MIN_ALIGN, false)
}

/// `calloc` after the overflow check: memory known zero is not written.
pub fn alloc_zeroed(size: usize) -> *mut u8 {
    if size > MAX_CLASS_SIZE {
        return alloc_big(size, MIN_ALIGN, true);
    }
    let (p, fresh) = alloc_class(class_of(size));
    if !p.is_null() {
        // SAFETY: `p` is a block of at least `size` bytes; a fresh block's
        // link word is its only non-zero byte.
        unsafe {
            if fresh {
                p.cast::<usize>().write(0);
            } else {
                ptr::write_bytes(p, 0, size);
            }
        }
    }
    p
}

/// `memalign` for any power-of-two `align`.
pub fn alloc_aligned(align: usize, size: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    if align <= MIN_ALIGN {
        return alloc(size);
    }
    if size <= MAX_CLASS_SIZE && align <= SLICE_SIZE {
        // Blocks sit at multiples of their size from a slice boundary, so a
        // class whose size `align` divides aligns every block.
        let mut class = class_of(size);
        while class <= CLASSES {
            let block = class_size(class);
            if block % align == 0 {
                return alloc_class(class).0;
            }
            if block >= size + align {
                break;
            }
            class += 1;
        }
        let padded = size + align - MIN_ALIGN;
        if padded <= MAX_CLASS_SIZE {
            let p = alloc(padded);
            if p.is_null() {
                return p;
            }
            let aligned = (p as usize).next_multiple_of(align) as *mut u8;
            if aligned != p {
                // SAFETY: `p` is live, so its span is too; the span is this
                // thread's, and the flag is published with the pointer.
                unsafe {
                    (*page_of(segment_of(p), p))
                        .has_aligned
                        .store(true, Ordering::Relaxed);
                }
            }
            return aligned;
        }
    }
    alloc_big(size, align, false)
}

/// `free`.
///
/// # Safety
/// `p` is null or a live allocation of this allocator.
#[inline]
pub unsafe fn free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let seg = segment_of(p);
    let page = page_of(seg, p);
    let owner = (*page).heap.load(Ordering::Relaxed);
    if owner == *os::heap_slot() && !owner.is_null() {
        let block = block_of(page, p);
        (*block).next = (*page).local_free;
        publish();
        (*page).local_free = block;
        (*page).used -= 1;
        if ((*page).used == 0 || (*page).in_full)
            && ((*page).in_full || !keeps_when_empty(owner, page))
        {
            free_local_slow(owner, page);
        }
    } else if owner.is_null() {
        free_big(page);
    } else {
        free_foreign(owner, page, block_of(page, p));
    }
}

/// `malloc_usable_size`.
///
/// # Safety
/// `p` is null or a live allocation of this allocator.
pub unsafe fn usable_size(p: *mut u8) -> usize {
    if p.is_null() {
        return 0;
    }
    usable_in(page_of(segment_of(p), p), p)
}

/// `realloc`: in place while the block still fits and is not twice too big.
///
/// # Safety
/// `p` is null or a live allocation of this allocator.
pub unsafe fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    if p.is_null() {
        return alloc(size);
    }
    if size == 0 {
        free(p);
        return null_mut();
    }
    let page = page_of(segment_of(p), p);
    let usable = usable_in(page, p);
    let arena_span = (*page).heap.load(Ordering::Relaxed).is_null() && !segment::is_huge(page);
    if size <= usable {
        if size >= usable / 2 {
            return p;
        }
        if arena_span && size > MAX_CLASS_SIZE {
            segment::shrink_span(page, size.div_ceil(SLICE_SIZE));
            return p;
        }
        if segment::is_huge(page) && size > LARGE_MAX {
            return p;
        }
    } else if arena_span && size <= LARGE_MAX && segment::grow_span(page, size.div_ceil(SLICE_SIZE))
    {
        return p;
    }
    let fresh = alloc(size);
    if fresh.is_null() {
        return if size <= usable { p } else { fresh };
    }
    ptr::copy_nonoverlapping(p, fresh, usable.min(size));
    free(p);
    fresh
}

pub fn heap_stats() -> HeapStats {
    let (arena_size, cached_size, largest_free, direct_count) = segment::stats();
    HeapStats {
        arena_size,
        cached_size,
        largest_free,
        direct_count,
    }
}

/// Hand a finished thread's heap to the pool, after returning every span it
/// no longer uses.
///
/// # Safety
/// `slot` is the calling thread's heap slot, and the thread allocates
/// nothing afterwards.
pub unsafe fn release_thread_heap(slot: *mut *mut Heap) {
    let heap = *slot;
    if heap.is_null() {
        return;
    }
    os::lock(&(*heap).lock);
    collect_all(heap);
    os::unlock(&(*heap).lock);
    *slot = null_mut();
    RegistryGuard::lock().pool_push(heap);
}

/// The allocator's locks, held across `fork` so the child inherits every
/// heap and the arena consistent.
pub struct ForkGuard(());

pub fn fork_prepare() -> ForkGuard {
    os::lock(&REGISTRY_LOCK);
    // SAFETY: the registry lock is held, so the list is stable.
    unsafe {
        let mut heap = (*REGISTRY.get()).all;
        while !heap.is_null() {
            os::lock(&(*heap).lock);
            heap = (*heap).next_all;
        }
    }
    segment::lock_for_fork();
    ForkGuard(())
}

impl ForkGuard {
    /// Release the locks. In the child only the forking thread survives, so
    /// every heap another thread owned or was cleaning goes to the pool.
    pub fn finish(self, child: bool) {
        // SAFETY: every lock `fork_prepare` took is still held.
        unsafe {
            let registry = &mut *REGISTRY.get();
            if child {
                let current = *os::heap_slot();
                // Cleared so it is not handed out again; pooled below.
                os::take_orphan();
                let mut heap = registry.all;
                while !heap.is_null() {
                    if heap != current && (*heap).state.load(Ordering::Relaxed) != POOLED {
                        registry.pool_push(heap);
                    }
                    heap = (*heap).next_all;
                }
            }
            segment::unlock_after_fork();
            let mut heap = registry.all;
            while !heap.is_null() {
                os::unlock(&(*heap).lock);
                heap = (*heap).next_all;
            }
        }
        os::unlock(&REGISTRY_LOCK);
    }
}

// ---------------------------------------------------------------------------
// Class allocation
// ---------------------------------------------------------------------------

/// A block of `class` and whether it is zero past its link word.
#[inline]
fn alloc_class(class: usize) -> (*mut u8, bool) {
    // SAFETY: as in `alloc`.
    unsafe {
        let heap = *os::heap_slot();
        if !heap.is_null() {
            let page = (*heap).queues[class].first;
            if !page.is_null() {
                let block = (*page).free;
                if !block.is_null() {
                    (*page).free = (*block).next;
                    (*page).used += 1;
                    return (block.cast(), (*page).free_fresh);
                }
            }
        }
    }
    alloc_class_slow(class)
}

#[cold]
#[inline(never)]
fn alloc_class_slow(class: usize) -> (*mut u8, bool) {
    // SAFETY: the slot is this thread's; the heap in it is this thread's,
    // and its queues change only under its lock.
    unsafe {
        let slot = os::heap_slot();
        let mut heap = *slot;
        if heap.is_null() {
            heap = acquire_heap();
            if heap.is_null() {
                return (null_mut(), false);
            }
            *slot = heap;
        }
        (*heap).slow_count = (*heap).slow_count.wrapping_add(1);
        if (*heap).slow_count % CLEAN_INTERVAL == 0 && POOL_DIRTY.load(Ordering::Relaxed) {
            clean_pool();
        }
        // Most refills need only the head span: its own frees, the ones
        // other threads made, or a further stretch of never-used blocks.
        let page = (*heap).queues[class].first;
        if !page.is_null() {
            collect(page);
            if (*page).free.is_null() && (*page).capacity < (*page).reserved {
                extend(page);
            }
            let block = (*page).free;
            if !block.is_null() {
                (*page).free = (*block).next;
                (*page).used += 1;
                return (block.cast(), (*page).free_fresh);
            }
        }
        os::lock(&(*heap).lock);
        let page = find_page(heap, class);
        os::unlock(&(*heap).lock);
        if page.is_null() {
            return (null_mut(), false);
        }
        let block = (*page).free;
        (*page).free = (*block).next;
        (*page).used += 1;
        (block.cast(), (*page).free_fresh)
    }
}

/// A span of `class` with a free block, first in its queue.
///
/// # Safety
/// Caller holds `heap`'s lock and owns it.
unsafe fn find_page(heap: *mut Heap, class: usize) -> *mut Page {
    drain_notify(heap);
    let queue = &raw mut (*heap).queues[class];
    let mut page = (*queue).first;
    while !page.is_null() {
        collect(page);
        if !(*page).free.is_null() {
            break;
        }
        if (*page).capacity < (*page).reserved {
            extend(page);
            break;
        }
        if mark_full(page) {
            let next = (*page).next;
            queue_remove(queue, page);
            queue_push_back(&raw mut (*heap).full, page);
            (*page).in_full = true;
            page = next;
        }
    }
    if page.is_null() {
        page = new_page(heap, class);
        if page.is_null() {
            return page;
        }
        queue_push_front(queue, page);
        extend(page);
    } else if (*queue).first != page {
        queue_remove(queue, page);
        queue_push_front(queue, page);
    }
    page
}

unsafe fn new_page(heap: *mut Heap, class: usize) -> *mut Page {
    let slices = span_slices(class);
    let page = segment::alloc_span(slices);
    if page.is_null() {
        return page;
    }
    let block_size = class_size(class);
    (*page).free = null_mut();
    (*page).local_free = null_mut();
    (*page).used = 0;
    (*page).capacity = 0;
    (*page).reserved = ((slices << SLICE_SHIFT) / block_size) as u32;
    (*page).class = class as u8;
    (*page).in_full = false;
    (*page).free_fresh = false;
    (*page).has_aligned.store(false, Ordering::Relaxed);
    (*page).block_size = block_size;
    (*page).heap.store(heap, Ordering::Relaxed);
    (*page).xthread.store(0, Ordering::Relaxed);
    (*page).next = null_mut();
    (*page).prev = null_mut();
    (*page).notify_next = null_mut();
    page
}

/// Order two stores to a heap's own lists as a `fork` in another thread may
/// observe them: an interrupted update may lose blocks, never list one twice.
#[inline(always)]
fn publish() {
    compiler_fence(Ordering::Release);
}

/// Carve the next stretch of never-used blocks into the empty free list.
unsafe fn extend(page: *mut Page) {
    let size = (*page).block_size;
    let count = ((*page).reserved - (*page).capacity).min((EXTEND_BYTES / size).max(1) as u32);
    let first = (*page).start.add((*page).capacity as usize * size);
    let mut block = first;
    for _ in 1..count {
        let next = block.add(size);
        (*block.cast::<Block>()).next = next.cast();
        block = next;
    }
    (*block.cast::<Block>()).next = null_mut();
    (*page).capacity += count;
    (*page).free_fresh = (*page).mem_fresh;
    publish();
    (*page).free = first.cast();
}

/// Refill an empty free list from the owner's frees, and take in whatever
/// other threads freed.
unsafe fn collect(page: *mut Page) {
    if (*page).free.is_null() && !(*page).local_free.is_null() {
        let list = (*page).local_free;
        (*page).local_free = null_mut();
        (*page).free_fresh = false;
        publish();
        (*page).free = list;
    }
    if (*page).xthread.load(Ordering::Relaxed) & !FLAGS == 0 {
        return;
    }
    let taken = (*page).xthread.fetch_and(FLAGS, Ordering::Acquire) & !FLAGS;
    let head = taken as *mut Block;
    let mut tail = head;
    let mut count = 1;
    while !(*tail).next.is_null() {
        tail = (*tail).next;
        count += 1;
    }
    (*tail).next = (*page).free;
    (*page).used -= count;
    (*page).free_fresh = false;
    publish();
    (*page).free = head;
}

/// Flag a span with no free block as full, unless a foreign free just
/// arrived; from then on the first foreign free notifies the owner.
unsafe fn mark_full(page: *mut Page) -> bool {
    let word = (*page).xthread.load(Ordering::Relaxed);
    word & !FLAGS == 0
        && (*page)
            .xthread
            .compare_exchange(word, word | FULL, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

// ---------------------------------------------------------------------------
// Freeing
// ---------------------------------------------------------------------------

#[inline]
unsafe fn block_of(page: *mut Page, p: *mut u8) -> *mut Block {
    if !(*page).has_aligned.load(Ordering::Relaxed) {
        return p.cast();
    }
    let offset = p as usize - (*page).start as usize;
    (*page)
        .start
        .add(offset - offset % (*page).block_size)
        .cast()
}

unsafe fn usable_in(page: *mut Page, p: *mut u8) -> usize {
    if (*page).heap.load(Ordering::Relaxed).is_null() {
        return (*page).start as usize + (*page).block_size - p as usize;
    }
    let block = block_of(page, p);
    (*page).block_size - (p as usize - block as usize)
}

#[cold]
#[inline(never)]
unsafe fn free_local_slow(heap: *mut Heap, page: *mut Page) {
    os::lock(&(*heap).lock);
    if (*page).in_full {
        unfull(heap, page);
    }
    if (*page).used == 0 {
        retire_if_spare(heap, page);
    }
    os::unlock(&(*heap).lock);
}

#[inline(never)]
unsafe fn free_foreign(heap: *mut Heap, page: *mut Page, block: *mut Block) {
    let mut word = (*page).xthread.load(Ordering::Relaxed);
    let notify = loop {
        (*block).next = (word & !FLAGS) as *mut Block;
        let notify = word & FLAGS == FULL;
        let new = block as usize | (word & FLAGS) | if notify { NOTIFIED } else { 0 };
        match (*page)
            .xthread
            .compare_exchange_weak(word, new, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => break notify,
            Err(now) => word = now,
        }
    };
    if notify {
        // `NOTIFIED` pins the span in its heap until the owner drains it.
        let mut head = (*heap).notify.load(Ordering::Relaxed);
        loop {
            (*page).notify_next = head;
            match (*heap).notify.compare_exchange_weak(
                head,
                page,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
    }
    if (*heap).state.load(Ordering::Relaxed) != OWNED {
        POOL_DIRTY.store(true, Ordering::Relaxed);
    }
}

#[inline(never)]
unsafe fn free_big(page: *mut Page) {
    if segment::is_huge(page) {
        segment::free_huge(page);
    } else {
        segment::free_span(page);
    }
}

// ---------------------------------------------------------------------------
// Heap upkeep, under the heap's lock
// ---------------------------------------------------------------------------

/// Put back in their queues the full spans that foreign frees reached.
unsafe fn drain_notify(heap: *mut Heap) {
    if (*heap).notify.load(Ordering::Relaxed).is_null() {
        return;
    }
    let mut page = (*heap).notify.swap(null_mut(), Ordering::Acquire);
    while !page.is_null() {
        let next = (*page).notify_next;
        (*page).xthread.fetch_and(!FLAGS, Ordering::AcqRel);
        if (*page).in_full {
            queue_remove(&raw mut (*heap).full, page);
            (*page).in_full = false;
            queue_push_back(&raw mut (*heap).queues[(*page).class as usize], page);
        }
        page = next;
    }
}

unsafe fn unfull(heap: *mut Heap, page: *mut Page) {
    queue_remove(&raw mut (*heap).full, page);
    (*page).in_full = false;
    // A pending notification keeps `NOTIFIED`: the drain still visits it.
    (*page).xthread.fetch_and(!FULL, Ordering::Relaxed);
    queue_push_back(&raw mut (*heap).queues[(*page).class as usize], page);
}

/// Return an empty span to its segment unless it is a small one its class
/// allocates from next — kept so a class used one block at a time does not
/// take the arena lock twice per block — or a notification still points at
/// it.
unsafe fn retire_if_spare(heap: *mut Heap, page: *mut Page) {
    if !keeps_when_empty(heap, page) {
        retire(&raw mut (*heap).queues[(*page).class as usize], page);
    }
}

#[inline]
unsafe fn keeps_when_empty(heap: *mut Heap, page: *mut Page) -> bool {
    (*heap).queues[(*page).class as usize].first == page && (*page).slice_count <= KEEP_EMPTY_SLICES
}

unsafe fn retire(queue: *mut Queue, page: *mut Page) {
    if (*page).xthread.load(Ordering::Acquire) & NOTIFIED != 0 {
        return;
    }
    queue_remove(queue, page);
    (*page).heap.store(null_mut(), Ordering::Relaxed);
    segment::free_span(page);
}

/// Collect every span and retire the empty ones.
unsafe fn collect_all(heap: *mut Heap) {
    drain_notify(heap);
    for class in 1..=CLASSES {
        let queue = &raw mut (*heap).queues[class];
        let mut page = (*queue).first;
        while !page.is_null() {
            let next = (*page).next;
            collect(page);
            if (*page).used == 0 {
                retire(queue, page);
            }
            page = next;
        }
    }
}

// ---------------------------------------------------------------------------
// Heap lifetime
// ---------------------------------------------------------------------------

struct RegistryGuard;

impl RegistryGuard {
    fn lock() -> Self {
        os::lock(&REGISTRY_LOCK);
        RegistryGuard
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        os::unlock(&REGISTRY_LOCK);
    }
}

impl core::ops::Deref for RegistryGuard {
    type Target = Registry;

    fn deref(&self) -> &Registry {
        // SAFETY: the guard holds the registry lock.
        unsafe { &*REGISTRY.get() }
    }
}

impl core::ops::DerefMut for RegistryGuard {
    fn deref_mut(&mut self) -> &mut Registry {
        // SAFETY: the guard holds the registry lock.
        unsafe { &mut *REGISTRY.get() }
    }
}

impl Registry {
    /// Take the first pooled heap this cleaning pass has not seen yet.
    unsafe fn pool_take_uncleaned(&mut self, pass: u32) -> *mut Heap {
        let mut link: *mut *mut Heap = &raw mut self.pool;
        while !(*link).is_null() {
            let heap = *link;
            if (*heap).clean_pass != pass {
                *link = (*heap).next_pool;
                (*heap).clean_pass = pass;
                (*heap).state.store(CLEANING, Ordering::Relaxed);
                return heap;
            }
            link = &raw mut (*heap).next_pool;
        }
        null_mut()
    }

    unsafe fn pool_push(&mut self, heap: *mut Heap) {
        (*heap).state.store(POOLED, Ordering::Relaxed);
        (*heap).next_pool = self.pool;
        self.pool = heap;
    }
}

/// A heap for a thread that has none: the one it had before TLS, a pooled
/// one, or a new one.
fn acquire_heap() -> *mut Heap {
    let orphan = os::take_orphan();
    if !orphan.is_null() {
        return orphan;
    }
    let mut registry = RegistryGuard::lock();
    // SAFETY: under the registry lock, pooled heaps are nobody's.
    unsafe {
        let pooled = registry.pool;
        if !pooled.is_null() {
            registry.pool = (*pooled).next_pool;
            (*pooled).state.store(OWNED, Ordering::Relaxed);
            drop(registry);
            // What other threads freed into it while pooled goes back to the
            // arena now, not whenever this thread next wants that class.
            os::lock(&(*pooled).lock);
            collect_all(pooled);
            os::unlock(&(*pooled).lock);
            return pooled;
        }
        let size = size_of::<Heap>().next_multiple_of(64);
        if registry.spare_left < size {
            let chunk = os::map(HEAP_CHUNK);
            if chunk.is_null() {
                return null_mut();
            }
            registry.spare = chunk;
            registry.spare_left = HEAP_CHUNK;
        }
        let heap = registry.spare.cast::<Heap>();
        registry.spare = registry.spare.add(size);
        registry.spare_left -= size;
        (*heap).next_all = registry.all;
        registry.all = heap;
        heap
    }
}

/// Collect what foreign frees returned to pooled heaps, so their empty
/// spans serve other heaps before the arena grows. One heap at a time
/// leaves the pool, so the rest stay adoptable meanwhile.
fn clean_pool() {
    if !POOL_DIRTY.swap(false, Ordering::Relaxed) {
        return;
    }
    let pass = {
        let mut registry = RegistryGuard::lock();
        registry.clean_pass = registry.clean_pass.wrapping_add(1);
        registry.clean_pass
    };
    loop {
        let heap = {
            let mut registry = RegistryGuard::lock();
            // SAFETY: under the registry lock, pooled heaps are nobody's.
            unsafe { registry.pool_take_uncleaned(pass) }
        };
        if heap.is_null() {
            return;
        }
        // SAFETY: the heap left the pool and is this thread's until put
        // back; it is cleaned under its own lock, as `fork` expects.
        unsafe {
            os::lock(&(*heap).lock);
            collect_all(heap);
            os::unlock(&(*heap).lock);
            RegistryGuard::lock().pool_push(heap);
        }
    }
}

// ---------------------------------------------------------------------------
// Big objects
// ---------------------------------------------------------------------------

fn alloc_big(size: usize, align: usize, zero: bool) -> *mut u8 {
    if size <= LARGE_MAX && align <= SLICE_SIZE {
        let slices = size.div_ceil(SLICE_SIZE).max(1);
        let page = segment::alloc_span(slices);
        if page.is_null() {
            return null_mut();
        }
        // SAFETY: the span is ours until freed.
        unsafe {
            (*page).free = null_mut();
            (*page).local_free = null_mut();
            (*page).used = 1;
            (*page).capacity = 1;
            (*page).reserved = 1;
            (*page).class = 0;
            (*page).in_full = false;
            (*page).free_fresh = false;
            (*page).has_aligned.store(false, Ordering::Relaxed);
            (*page).block_size = slices << SLICE_SHIFT;
            (*page).heap.store(null_mut(), Ordering::Relaxed);
            (*page).xthread.store(0, Ordering::Relaxed);
            if zero && !(*page).mem_fresh {
                ptr::write_bytes((*page).start, 0, size);
            }
            return (*page).start;
        }
    }
    let (p, fresh) = segment::alloc_huge(size, align);
    if zero && !fresh && !p.is_null() {
        // SAFETY: the object is at least `size` bytes.
        unsafe { ptr::write_bytes(p, 0, size) };
    }
    p
}

// ---------------------------------------------------------------------------
// Page queues
// ---------------------------------------------------------------------------

unsafe fn queue_remove(queue: *mut Queue, page: *mut Page) {
    let (prev, next) = ((*page).prev, (*page).next);
    if prev.is_null() {
        (*queue).first = next;
    } else {
        (*prev).next = next;
    }
    if next.is_null() {
        (*queue).last = prev;
    } else {
        (*next).prev = prev;
    }
    (*page).prev = null_mut();
    (*page).next = null_mut();
}

unsafe fn queue_push_front(queue: *mut Queue, page: *mut Page) {
    (*page).prev = null_mut();
    (*page).next = (*queue).first;
    if (*queue).first.is_null() {
        (*queue).last = page;
    } else {
        (*(*queue).first).prev = page;
    }
    (*queue).first = page;
}

unsafe fn queue_push_back(queue: *mut Queue, page: *mut Page) {
    (*page).next = null_mut();
    (*page).prev = (*queue).last;
    if (*queue).last.is_null() {
        (*queue).first = page;
    } else {
        (*(*queue).last).next = page;
    }
    (*queue).last = page;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_maps_to_the_smallest_class_that_holds_it() {
        let mut previous = 0;
        for class in 1..=CLASSES {
            let size = class_size(class);
            assert!(size > previous && size % 16 == 0, "class {class}");
            previous = size;
        }
        for size in 0..=MAX_CLASS_SIZE {
            let class = class_of(size);
            assert!((1..=CLASSES).contains(&class));
            assert!(class_size(class) >= size, "{size}");
            assert!(class == 1 || class_size(class - 1) < size, "{size}");
            let waste = class_size(class) - size.max(1);
            assert!(size <= 128 || waste * 8 <= size, "{size} wastes {waste}");
        }
    }

    #[test]
    fn a_class_span_holds_enough_blocks() {
        for class in 1..=CLASSES {
            let slices = span_slices(class);
            assert!((1..SLICES).contains(&slices));
            let blocks = (slices << SLICE_SHIFT) / class_size(class);
            assert!(blocks >= 4, "class {class}");
            assert!(blocks < 1 << 16);
        }
    }
}
