//! A per-thread cache of small freed chunks in front of the arena's one lock.
//!
//! A multi-threaded compiler allocates and frees small objects on every
//! thread at once; with one lock around the whole arena each of those calls
//! is a potential futex sleep. A chunk freed here goes onto the freeing
//! thread's own list and the next allocation of its size class on that thread
//! takes it back, so the common case takes no lock at all — the design of
//! glibc's tcache, with its bin sizes and fill count.
//!
//! A cached chunk stays *allocated* as far as the arena can tell: its
//! neighbours cannot coalesce into it and its segment cannot be released, so
//! nothing the arena does while holding the lock can reach it. The chunk's
//! first data word links the list and its second carries [`KEY`], which is
//! what turns a second `free` of a cached chunk into an abort rather than a
//! list that hands the chunk out twice.

use core::ffi::c_void;
use core::ptr;

use super::chunk;
use super::dlmalloc::{ALLOCATOR, DlMalloc};
use crate::thread::tcb::Tcb;
use crate::thread::tls::tls_is_initialized;

/// Size classes, one per 16-byte step from the smallest chunk.
pub const BINS: usize = 64;
/// Chunks one class holds before a free goes to the arena instead.
const FILL: u8 = 7;
const LARGEST: usize = chunk::MIN_CHUNK_SIZE + (BINS - 1) * chunk::ALIGNMENT;

/// Stamped into a cached chunk's second data word. The address of a static
/// is enough: nothing forges it by accident, and a program that frees a
/// pointer twice is not an adversary.
static KEY_ANCHOR: u8 = 0;

#[inline]
fn key() -> usize {
    (&raw const KEY_ANCHOR) as usize ^ 0x5bd1_e995_5bd1_e995
}

#[repr(C)]
pub struct Tcache {
    heads: [*mut u8; BINS],
    counts: [u8; BINS],
}

impl Tcache {
    pub const fn new() -> Self {
        Self {
            heads: [ptr::null_mut(); BINS],
            counts: [0; BINS],
        }
    }
}

impl Default for Tcache {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn class_of(chunk_size: usize) -> Option<usize> {
    if !(chunk::MIN_CHUNK_SIZE..=LARGEST).contains(&chunk_size)
        || chunk_size & (chunk::ALIGNMENT - 1) != 0
    {
        return None;
    }
    Some((chunk_size - chunk::MIN_CHUNK_SIZE) / chunk::ALIGNMENT)
}

/// The calling thread's cache, once it has one: before TLS exists every call
/// goes to the arena.
#[inline]
fn current() -> Option<*mut Tcache> {
    if !tls_is_initialized() {
        return None;
    }
    // SAFETY: `tls_is_initialized` reports that `fs_base` holds a live TCB.
    Some(unsafe { &raw mut (*Tcb::current()).tcache })
}

/// A cached chunk for a `size`-byte request, if this thread holds one.
#[inline]
pub fn take(size: usize) -> Option<*mut c_void> {
    // `malloc(0)` is the arena's null, whatever this thread freed last.
    if size == 0 {
        return None;
    }
    let class = class_of(DlMalloc::request_size(size)?)?;
    let cache = current()?;
    // SAFETY: the cache is this thread's own, and every pointer on its lists
    // is a live arena chunk's data, put there by `put`.
    unsafe {
        let head = (*cache).heads[class];
        if head.is_null() {
            return None;
        }
        let words = head.cast::<usize>();
        (*cache).heads[class] = *words as *mut u8;
        (*cache).counts[class] -= 1;
        *words.add(1) = 0;
        Some(head.cast())
    }
}

/// Keep a freed chunk on this thread, answering whether it was taken.
///
/// # Safety
/// `ptr` must be a pointer `malloc` returned and nobody has freed since.
#[inline]
pub unsafe fn put(ptr: *mut c_void) -> bool {
    let Some(cache) = current() else {
        return false;
    };
    let data = ptr.cast::<u8>();
    // SAFETY: the caller hands over a live allocation, whose header precedes
    // it; an arena chunk's successor header is inside the same segment.
    let (size, mapped, in_use) = unsafe {
        let chunk_ptr = chunk::from_data_ptr(data);
        let mapped = chunk::is_mmap(chunk_ptr);
        let size = chunk::size(chunk_ptr);
        let in_use = !mapped && size != 0 && chunk::is_prev_in_use(chunk::next_physical(chunk_ptr));
        (size, mapped, in_use)
    };
    // A chunk the arena already holds free is a second `free`, which the
    // arena's own validation turns into a no-op; caching it would hand it out
    // twice.
    if mapped || !in_use {
        return false;
    }
    let Some(class) = class_of(size) else {
        return false;
    };
    // SAFETY: `cache` is this thread's; a chunk of `size` has at least two
    // data words.
    unsafe {
        let words = data.cast::<usize>();
        if *words.add(1) == key() && holds(&*cache, class, data) {
            const MSG: &[u8] = b"free(): double free of a cached chunk\n";
            let _ = <crate::pal::Sys as crate::pal::Pal>::write(2, MSG.as_ptr(), MSG.len());
            crate::signal::abort();
        }
        if (*cache).counts[class] >= FILL {
            return false;
        }
        *words = (*cache).heads[class] as usize;
        *words.add(1) = key();
        (*cache).heads[class] = data;
        (*cache).counts[class] += 1;
    }
    true
}

/// Whether `data` is already on `class`'s list: only asked when the key says
/// it may be, so a correct program never walks a list.
fn holds(cache: &Tcache, class: usize, data: *mut u8) -> bool {
    let mut cursor = cache.heads[class];
    while !cursor.is_null() {
        if cursor == data {
            return true;
        }
        // SAFETY: every list member is a cached chunk's data.
        cursor = unsafe { *cursor.cast::<*mut u8>() };
    }
    false
}

/// Give every chunk `tcb`'s cache holds back to the arena. Run by a thread on
/// its way out, after its destructors, so a detached thread — whose TCB
/// nobody frees — strands nothing.
///
/// # Safety
/// `tcb` must be the calling thread's own TCB, and the thread must allocate
/// nothing after this.
pub unsafe fn release(tcb: *mut Tcb) {
    // SAFETY: the cache is the caller's own.
    let cache = unsafe { &mut (*tcb).tcache };
    if cache.counts.iter().all(|&n| n == 0) {
        return;
    }
    let mut arena = ALLOCATOR.lock();
    for class in 0..BINS {
        let mut cursor = cache.heads[class];
        while !cursor.is_null() {
            // SAFETY: a cached chunk's first word links the list.
            let next = unsafe { *cursor.cast::<*mut u8>() };
            arena.dealloc(cursor.cast());
            cursor = next;
        }
        cache.heads[class] = ptr::null_mut();
        cache.counts[class] = 0;
    }
}
