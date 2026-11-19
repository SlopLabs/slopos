//! Large-allocation tier (> 2048 bytes → direct frame allocation).
//!
//! Regions come from the buddy as contiguous pages whose first page carries a
//! `LargeAllocHeader`. A freed region of up to [`EXACT_PAGES`] pages waits on
//! the free list of its size, threaded intrusively through the header, for
//! the next request of that size, while the lists hold no more than
//! [`CACHE_PAGES`] together; any other goes back to the buddy. A request is
//! never served a larger region: what it left unused would stay with the
//! allocation for as long as that lives.
//!
//! There is no external tracking table: `kfree` discriminates on the page
//! magic at the 4 KiB-aligned base (`super::page::page_kind_for`), which only
//! the allocator writes, so it never collides with
//! [`super::page::SLAB_MAGIC`].

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use slopos_ostd::lock_class;

use slopos_ostd::sync::{LOCK_LEVEL_ALLOCATOR, RawLink, SpinLock};

use super::page::{
    LARGE_FREE_MAGIC, LARGE_MAGIC, LargeAllocHeader, alloc_large_pages, free_large_pages,
    large_alloc_count_inc,
};
use super::poison::{POISON_FREED, poison_object_body};
use crate::paging_defs::PAGE_SIZE_4KB;

const SLAB_DEBUG: bool = false;

const EXACT_PAGES: usize = 16;

/// What the free lists hold together at most, 4 MiB: enough that a run of
/// allocations and frees stays off the buddy, and all that a burst of frees
/// keeps from every other user of memory.
pub(crate) const CACHE_PAGES: u32 = 1024;

pub(crate) struct LargeInner {
    free_lists: [RawLink<LargeAllocHeader>; EXACT_PAGES],
    cached_pages: u32,
}

impl LargeInner {
    pub(crate) const fn new() -> Self {
        Self {
            free_lists: [const { RawLink::null() }; EXACT_PAGES],
            cached_pages: 0,
        }
    }

    /// The list of regions of `pages`, when regions that size are kept.
    fn list_for(&self, pages: u32) -> Option<&RawLink<LargeAllocHeader>> {
        self.free_lists.get((pages as usize).checked_sub(1)?)
    }

    fn take(&mut self, pages: u32) -> Option<NonNull<LargeAllocHeader>> {
        let list = self.list_for(pages)?;
        let head = list.load()?;
        let next = RawLink::<LargeAllocHeader>::with_mut_at(Some(head), |h| h.next.load())?;
        list.store(next);
        self.cached_pages = self.cached_pages.saturating_sub(pages);
        Some(head)
    }
}

pub struct LargeAlloc {
    inner: SpinLock<LargeInner>,
    pub(crate) total_bytes_allocated: AtomicU64,
    pub(crate) total_bytes_freed: AtomicU64,
}

impl LargeAlloc {
    pub(crate) const fn new() -> Self {
        Self {
            inner: SpinLock::new(
                LargeInner::new(),
                lock_class!("LARGE_ALLOC", LOCK_LEVEL_ALLOCATOR),
            ),
            total_bytes_allocated: AtomicU64::new(0),
            total_bytes_freed: AtomicU64::new(0),
        }
    }

    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        let header_size = LargeAllocHeader::body_offset();
        let total = size.checked_add(header_size)?;
        let pages = total.div_ceil(PAGE_SIZE_4KB as usize) as u32;
        if pages == 0 {
            return None;
        }

        if let Some(curr) = self.inner.lock().take(pages) {
            RawLink::<LargeAllocHeader>::with_mut_at(Some(curr), |h| {
                h.magic = LARGE_MAGIC;
                h.size = size as u32;
                h.next = RawLink::null();
            });
            self.total_bytes_allocated
                .fetch_add(size as u64, Ordering::Relaxed);
            return Some(LargeAllocHeader::body_ptr(curr));
        }

        let (base, _paddr) = alloc_large_pages(pages)?;
        let Some(header_nn) = NonNull::new(base.as_ptr() as *mut LargeAllocHeader) else {
            return None;
        };
        RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
            h.magic = LARGE_MAGIC;
            h.pages = pages;
            h.size = size as u32;
            h._reserved = 0;
            h.next = RawLink::null();
        });
        large_alloc_count_inc();
        self.total_bytes_allocated
            .fetch_add(size as u64, Ordering::Relaxed);
        Some(LargeAllocHeader::body_ptr(header_nn))
    }

    /// The caller has already established, via the page magic at the 4 KiB
    /// aligned base, that `ptr` belongs to a large region.
    pub fn dealloc(&self, ptr: NonNull<u8>) {
        let base_addr = (ptr.as_ptr() as u64) & !(PAGE_SIZE_4KB - 1);
        let Some(header_nn) = NonNull::new(base_addr as *mut LargeAllocHeader) else {
            return;
        };

        let mut state = self.inner.lock();
        let snap = RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
            if h.magic != LARGE_MAGIC {
                return None;
            }
            h.magic = LARGE_FREE_MAGIC;
            let list = state
                .list_for(h.pages)
                .filter(|_| state.cached_pages.saturating_add(h.pages) <= CACHE_PAGES);
            let kept = list.is_some();
            if let Some(list) = list {
                h.next.store(list.load());
                list.store(Some(header_nn));
                state.cached_pages += h.pages;
            }
            Some((h.pages, h.size as u64, kept))
        })
        .flatten();
        let Some((pages, size, kept)) = snap else {
            return;
        };
        self.total_bytes_freed.fetch_add(size, Ordering::Relaxed);
        if !kept {
            // Off the lists, so nothing else names it: the buddy is entered
            // without the tier's lock.
            drop(state);
            free_large_pages(header_nn.cast(), pages);
            return;
        }

        if SLAB_DEBUG {
            let hdr_sz = LargeAllocHeader::body_offset();
            let total_bytes = (pages as usize) * PAGE_SIZE_4KB as usize;
            if total_bytes > hdr_sz {
                let body_len = total_bytes - hdr_sz;
                LargeAllocHeader::with_body_view_mut(header_nn, body_len, |body| {
                    poison_object_body(body, POISON_FREED)
                });
            }
        }
    }
}

/// How many pages the live region `ptr` was allocated from holds.
#[cfg(feature = "test-hooks")]
pub(crate) fn region_pages(ptr: NonNull<u8>) -> Option<u32> {
    let base_addr = (ptr.as_ptr() as u64) & !(PAGE_SIZE_4KB - 1);
    let header = NonNull::new(base_addr as *mut LargeAllocHeader);
    RawLink::<LargeAllocHeader>::with_mut_at(header, |h| h.pages)
}
