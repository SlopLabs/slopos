//! slibc's allocator on the host: its core, `slibc/src/mem/heap`, over an
//! `os` module that maps with the host's `mmap` and keeps each thread's heap
//! in a `thread_local!` whose destructor does what slibc's thread exit does.

#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

#[allow(dead_code, unsafe_op_in_unsafe_fn)]
#[path = "../../slibc/src/mem/heap/mod.rs"]
mod heap;

mod os {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

    use super::heap::{Heap, release_thread_heap};

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    pub static MAPS: AtomicUsize = AtomicUsize::new(0);
    pub static UNMAPS: AtomicUsize = AtomicUsize::new(0);

    struct Slot(Cell<*mut Heap>);

    impl Drop for Slot {
        fn drop(&mut self) {
            // SAFETY: the thread is exiting and allocates nothing more here.
            unsafe { release_thread_heap(self.0.as_ptr()) }
        }
    }

    thread_local! {
        static SLOT: Slot = const { Slot(Cell::new(null_mut())) };
    }

    pub fn map(len: usize) -> *mut u8 {
        MAPS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: an anonymous private mapping at an address of the kernel's
        // choosing.
        let p = unsafe { mmap(null_mut(), len, 3, 0x22, -1, 0) };
        if p as isize == -1 {
            null_mut()
        } else {
            p.cast()
        }
    }

    pub unsafe fn unmap(p: *mut u8, len: usize) {
        UNMAPS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(unsafe { munmap(p.cast(), len) }, 0);
    }

    pub fn heap_slot() -> *mut *mut Heap {
        SLOT.with(|s| s.0.as_ptr())
    }

    pub fn take_orphan() -> *mut Heap {
        null_mut()
    }

    /// What a thread that vanished in a `fork` child leaves: a slot nobody
    /// releases.
    pub fn forget_heap() {
        SLOT.with(|s| s.0.set(null_mut()));
    }

    pub fn lock(state: &AtomicI32) {
        while state
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::thread::yield_now();
        }
    }

    pub fn unlock(state: &AtomicI32) {
        state.store(0, Ordering::Release);
    }

    pub fn fatal(msg: &str) -> ! {
        panic!("{msg}")
    }
}

use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier, Mutex, MutexGuard, mpsc};

use heap::{LARGE_MAX, MAX_CLASS_SIZE, SEGMENT_SIZE};

/// The allocator is one per process, and several tests read its totals.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Mostly small, sometimes medium or large, rarely huge.
    fn size(&mut self) -> usize {
        match self.below(1000) {
            0..=799 => self.below(257),
            800..=949 => self.below(8193),
            950..=993 => self.below(MAX_CLASS_SIZE + 1),
            994..=998 => MAX_CLASS_SIZE + 1 + self.below(LARGE_MAX - MAX_CLASS_SIZE),
            _ => LARGE_MAX + 1 + self.below(3 * SEGMENT_SIZE),
        }
    }
}

fn byte(tag: u64, i: usize) -> u8 {
    (tag.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8 ^ (i as u8).wrapping_mul(31)
}

/// Offsets to stamp in a block: all of a short one, the ends and a stride
/// through a long one.
fn offsets(len: usize) -> impl Iterator<Item = usize> {
    let stride = if len <= 1024 { 1 } else { 509 };
    (0..len.min(256))
        .chain((256..len).step_by(stride))
        .chain(len.saturating_sub(64).max(256)..len)
}

unsafe fn fill(p: *mut u8, len: usize, tag: u64) {
    for i in offsets(len) {
        unsafe { *p.add(i) = byte(tag, i) };
    }
}

unsafe fn check(p: *const u8, len: usize, tag: u64) {
    unsafe { check_prefix(p, len, len, tag) }
}

/// The bytes of a `len`-byte block stamped with `tag` that survive in its
/// first `keep` bytes.
unsafe fn check_prefix(p: *const u8, len: usize, keep: usize, tag: u64) {
    for i in offsets(len).filter(|&i| i < keep) {
        let got = unsafe { *p.add(i) };
        assert_eq!(
            got,
            byte(tag, i),
            "block {p:p} len {len} tag {tag} offset {i}"
        );
    }
}

#[derive(Clone, Copy)]
struct Live {
    p: *mut u8,
    len: usize,
    tag: u64,
}

// SAFETY: a `Live` is handed between threads whole, and only one holds it.
unsafe impl Send for Live {}

fn allocate(len: usize, tag: u64) -> Live {
    let p = heap::alloc(len);
    assert!(!p.is_null(), "alloc({len})");
    assert_eq!(p as usize % heap::MIN_ALIGN, 0);
    // SAFETY: `p` is live.
    unsafe {
        assert!(heap::usable_size(p) >= len.max(1));
        fill(p, len, tag);
    }
    Live { p, len, tag }
}

fn release(live: Live) {
    // SAFETY: `live` is a live allocation nobody else holds.
    unsafe {
        check(live.p, live.len, live.tag);
        heap::free(live.p);
    }
}

#[test]
fn every_small_size_is_aligned_usable_and_distinct() {
    let _serial = serial();
    let mut live = Vec::new();
    for size in 0..=4096 {
        live.push(allocate(size, size as u64));
    }
    let mut spans: Vec<(usize, usize)> = live
        .iter()
        .map(|l| {
            (
                l.p as usize,
                l.p as usize + unsafe { heap::usable_size(l.p) },
            )
        })
        .collect();
    spans.sort();
    for pair in spans.windows(2) {
        assert!(pair[0].1 <= pair[1].0, "overlap {:x?}", pair);
    }
    for l in live {
        release(l);
    }
}

#[test]
fn usable_bytes_are_all_writable_without_touching_neighbours() {
    let _serial = serial();
    let mut rng = Rng(7);
    let mut live = Vec::new();
    for round in 0..20_000u64 {
        let len = rng.size().min(LARGE_MAX);
        let p = heap::alloc(len);
        // SAFETY: `p` is live for `usable_size` bytes.
        unsafe {
            let usable = heap::usable_size(p);
            assert!(usable >= len);
            fill(p, usable, round);
            live.push(Live {
                p,
                len: usable,
                tag: round,
            });
        }
        if rng.below(3) == 0 && !live.is_empty() {
            let at = rng.below(live.len());
            release(live.swap_remove(at));
        }
    }
    for l in live {
        release(l);
    }
}

#[test]
fn random_single_thread_churn() {
    let _serial = serial();
    let mut rng = Rng(0x1234_5678);
    let mut slots: Vec<Option<Live>> = vec![None; 4096];
    for op in 0..2_000_000u64 {
        let at = rng.below(slots.len());
        match slots[at].take() {
            None => slots[at] = Some(allocate(rng.size(), op)),
            Some(l) if rng.below(4) == 0 => {
                let len = rng.size().max(1);
                // SAFETY: `l` is live.
                unsafe {
                    check(l.p, l.len, l.tag);
                    let p = heap::realloc(l.p, len);
                    assert!(!p.is_null());
                    assert!(heap::usable_size(p) >= len);
                    check_prefix(p, l.len, len, l.tag);
                    fill(p, len, op);
                    slots[at] = Some(Live { p, len, tag: op });
                }
            }
            Some(l) => release(l),
        }
    }
    for l in slots.into_iter().flatten() {
        release(l);
    }
}

#[test]
fn calloc_zeroes_reused_memory_and_trusts_fresh_memory() {
    let _serial = serial();
    let sizes = [
        0,
        1,
        15,
        16,
        17,
        100,
        1000,
        4096,
        20_000,
        MAX_CLASS_SIZE,
        MAX_CLASS_SIZE + 1,
        LARGE_MAX,
        LARGE_MAX + 1,
        3 * SEGMENT_SIZE + 5,
    ];
    for round in 0..4 {
        let dirty: Vec<*mut u8> = sizes
            .iter()
            .flat_map(|&s| (0..8).map(move |_| s))
            .map(|s| {
                let p = heap::alloc(s);
                // SAFETY: `p` has `usable_size` bytes.
                unsafe { std::ptr::write_bytes(p, 0xa5, heap::usable_size(p)) };
                p
            })
            .collect();
        for p in dirty {
            unsafe { heap::free(p) };
        }
        let zeroed: Vec<(*mut u8, usize)> = sizes
            .iter()
            .flat_map(|&s| (0..8).map(move |_| s))
            .map(|s| (heap::alloc_zeroed(s), s))
            .collect();
        for (p, s) in zeroed {
            assert!(!p.is_null());
            // SAFETY: `p` has at least `s` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(p, s) };
            assert!(bytes.iter().all(|&b| b == 0), "calloc({s}) round {round}");
            unsafe { heap::free(p) };
        }
    }
}

#[test]
fn every_power_of_two_alignment() {
    let _serial = serial();
    let mut shift = 0;
    while 1usize << shift <= 2 * SEGMENT_SIZE {
        let align = 1usize << shift;
        for size in [
            0,
            1,
            8,
            100,
            align.saturating_sub(1),
            align,
            align + 1,
            3 * align,
            100_000,
            MAX_CLASS_SIZE - 1,
            LARGE_MAX + 7,
        ] {
            let p = heap::alloc_aligned(align, size);
            assert!(!p.is_null(), "memalign({align}, {size})");
            assert_eq!(p as usize % align, 0, "memalign({align}, {size})");
            // SAFETY: `p` is live for at least `size` bytes.
            unsafe {
                assert!(heap::usable_size(p) >= size);
                fill(p, size, size as u64);
                check(p, size, size as u64);
            }
            let (tx, rx) = mpsc::channel::<Live>();
            let live = Live {
                p,
                len: size,
                tag: size as u64,
            };
            if size % 2 == 0 {
                // An interior pointer freed by a thread that does not own it.
                tx.send(live).unwrap();
                std::thread::spawn(move || release(rx.recv().unwrap()))
                    .join()
                    .unwrap();
            } else {
                release(live);
            }
        }
        shift += 1;
    }
}

#[test]
fn realloc_keeps_contents_across_every_tier() {
    let _serial = serial();
    let mut len = 1;
    let mut live = allocate(len, 1);
    let mut tag = 1;
    let grow = |len: usize| (len * 3 / 2 + 1).max(len + 1);
    while len < 3 * SEGMENT_SIZE {
        let next = grow(len);
        // SAFETY: `live` is live.
        unsafe {
            let p = heap::realloc(live.p, next);
            assert!(!p.is_null());
            check(p, len, tag);
            tag += 1;
            fill(p, next, tag);
            live = Live { p, len: next, tag };
        }
        len = next;
    }
    while len > 1 {
        let next = len / 3;
        unsafe {
            let p = heap::realloc(live.p, next.max(1));
            assert!(!p.is_null());
            check_prefix(p, len, next, tag);
            fill(p, next, tag);
            live = Live { p, len: next, tag };
        }
        len = next;
    }
    release(live);
    unsafe {
        let minimal = heap::realloc(std::ptr::null_mut(), 0);
        assert!(!minimal.is_null());
        heap::free(minimal);
        let p = heap::alloc(10);
        assert!(heap::realloc(p, 0).is_null());
    }
}

#[test]
fn a_large_span_grows_in_place_when_its_neighbour_is_free() {
    let _serial = serial();
    let len = MAX_CLASS_SIZE + 1;
    let a = allocate(len, 3);
    // SAFETY: `a` is live.
    unsafe {
        let p = heap::realloc(a.p, LARGE_MAX);
        check(p, len, 3);
        if p == a.p {
            assert!(heap::usable_size(p) >= LARGE_MAX);
        }
        release(Live { p, len, tag: 3 });
    }
}

#[test]
fn huge_mappings_are_reused_rather_than_remapped() {
    let _serial = serial();
    let size = 3 * SEGMENT_SIZE;
    let before = heap::heap_stats().direct_count;
    let first = allocate(size, 9);
    assert_eq!(heap::heap_stats().direct_count, before + 1);
    release(first);
    assert_eq!(heap::heap_stats().direct_count, before);
    let maps = os::MAPS.load(Ordering::Relaxed);
    let unmaps = os::UNMAPS.load(Ordering::Relaxed);
    for round in 0..50 {
        release(allocate(size - round * 4096, round as u64));
    }
    assert_eq!(os::MAPS.load(Ordering::Relaxed), maps);
    assert_eq!(os::UNMAPS.load(Ordering::Relaxed), unmaps);
}

#[test]
fn segments_empty_back_into_the_cache() {
    let _serial = serial();
    let before = heap::heap_stats();
    let blocks: Vec<Live> = (0..64).map(|i| allocate(LARGE_MAX, i)).collect();
    let peak = heap::heap_stats();
    assert!(peak.arena_size >= before.arena_size + 32 * SEGMENT_SIZE);
    for b in blocks {
        release(b);
    }
    let after = heap::heap_stats();
    assert!(
        after.arena_size <= before.arena_size,
        "{after:?} {before:?}"
    );
}

/// Threads allocate, hand blocks to each other and free what they are
/// handed, in rounds of short-lived threads, so heaps are abandoned and
/// adopted throughout. Memory must reach a steady state, not grow per round.
#[test]
fn threads_free_each_others_blocks() {
    let _serial = serial();
    const THREADS: usize = 8;
    let mut after_round = Vec::new();
    for round in 0..8u64 {
        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..THREADS).map(|_| mpsc::channel::<Live>()).unzip();
        let senders = Arc::new(senders);
        let barrier = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = receivers
            .into_iter()
            .enumerate()
            .map(|(me, inbox)| {
                let senders = Arc::clone(&senders);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut rng = Rng(round * 97 + me as u64 + 1);
                    let mut mine = Vec::new();
                    for op in 0..60_000u64 {
                        let tag = (me as u64) << 48 | op;
                        let mut len = rng.size();
                        if len > LARGE_MAX {
                            len = rng.below(512);
                        }
                        match rng.below(10) {
                            0..=2 => mine.push(allocate(len, tag)),
                            3..=4 => {
                                let to = rng.below(THREADS);
                                let _ = senders[to].send(allocate(len, tag));
                            }
                            5..=7 => {
                                if !mine.is_empty() {
                                    let at = rng.below(mine.len());
                                    release(mine.swap_remove(at));
                                }
                            }
                            _ => {
                                while let Ok(l) = inbox.try_recv() {
                                    release(l);
                                }
                            }
                        }
                    }
                    barrier.wait();
                    while let Ok(l) = inbox.try_recv() {
                        release(l);
                    }
                    // Half the survivors outlive their thread, freed by a
                    // thread that owns none of them.
                    let outliving = mine.split_off(mine.len() / 2);
                    for l in mine {
                        release(l);
                    }
                    outliving
                })
            })
            .collect();
        let leftovers: Vec<Live> = workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        for l in leftovers {
            release(l);
        }
        drop(senders);
        after_round.push(heap::heap_stats().arena_size);
    }
    // Threads enough to adopt every pooled heap at once, and so collect what
    // was freed into it after its owner left; then nothing is held at all.
    let barrier = Arc::new(Barrier::new(32));
    let adopters: Vec<_> = (0..32)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                release(allocate(8, 1));
                barrier.wait();
            })
        })
        .collect();
    for a in adopters {
        a.join().unwrap();
    }
    let last = heap::heap_stats().arena_size;
    assert!(
        last <= 2 * SEGMENT_SIZE,
        "arena {last} after rounds {after_round:?}"
    );
}

/// A thread that exits holding blocks leaves its heap to the pool; once the
/// blocks are freed elsewhere, the pool is cleaned and its spans reused.
#[test]
fn an_exited_threads_memory_comes_back() {
    let _serial = serial();
    let before = heap::heap_stats().arena_size;
    let blocks = std::thread::spawn(|| {
        (0..10_000u64)
            .map(|i| allocate(1024, i))
            .collect::<Vec<_>>()
    })
    .join()
    .unwrap();
    let grown = heap::heap_stats().arena_size;
    for b in blocks {
        release(b);
    }
    // Enough slow paths that one of them cleans the pool: a block per refill
    // at this size.
    let mut ring: Vec<Option<Live>> = vec![None; 32];
    for i in 0..4096u64 {
        if let Some(old) = ring[i as usize % 32].replace(allocate(4096, i)) {
            release(old);
        }
    }
    for b in ring.into_iter().flatten() {
        release(b);
    }
    let after = heap::heap_stats().arena_size;
    assert!(
        after <= before + 2 * SEGMENT_SIZE,
        "before {before} grown {grown} after {after}"
    );
}

/// What a `fork` child sees: the locks held across the call, and every heap
/// but the forking thread's owned by a thread that is gone. Those heaps are
/// pooled, and another thread adopts them.
#[test]
fn heaps_of_threads_lost_to_fork_are_adopted() {
    let _serial = serial();
    let (ready_tx, ready_rx) = mpsc::channel::<Vec<Live>>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let vanished = std::thread::spawn(move || {
        let blocks: Vec<Live> = (0..5000u64)
            .map(|i| allocate(48 + (i % 7) as usize * 16, i))
            .collect();
        let kept: Vec<Live> = (0..100u64).map(|i| allocate(64, i + 10_000)).collect();
        ready_tx.send(blocks).unwrap();
        go_rx.recv().unwrap();
        for b in kept {
            // SAFETY: the pattern is still intact under the child's pool.
            unsafe { check(b.p, b.len, b.tag) };
        }
        os::forget_heap();
    });
    let blocks = ready_rx.recv().unwrap();
    let churners: Vec<_> = (0..3)
        .map(|t| {
            std::thread::spawn(move || {
                let mut rng = Rng(t + 5);
                for op in 0..20_000 {
                    let l = allocate(rng.below(2000), op);
                    release(l);
                }
            })
        })
        .collect();
    for _ in 0..200 {
        heap::fork_prepare().finish(false);
    }
    for c in churners {
        c.join().unwrap();
    }
    heap::fork_prepare().finish(true);
    go_tx.send(()).unwrap();
    vanished.join().unwrap();
    for b in blocks {
        release(b);
    }
    let adopted = std::thread::spawn(|| {
        let blocks: Vec<Live> = (0..20_000u64)
            .map(|i| allocate(48 + (i % 7) as usize * 16, i))
            .collect();
        for b in blocks {
            release(b);
        }
    });
    adopted.join().unwrap();
}

#[test]
#[ignore = "benchmark: cargo test --release -p slopos-slibc-core --test malloc -- --ignored --nocapture"]
fn bench() {
    use std::time::Instant;
    let _serial = serial();

    const N: usize = 5_000_000;
    let start = Instant::now();
    for i in 0..N {
        let p = heap::alloc(16 + (i & 0xff));
        unsafe { heap::free(std::hint::black_box(p)) };
    }
    let lifo = start.elapsed().as_nanos() as f64 / N as f64;

    let mut ring = vec![std::ptr::null_mut(); 1024];
    let start = Instant::now();
    for i in 0..N {
        let slot = &mut ring[i & 1023];
        unsafe { heap::free(*slot) };
        *slot = heap::alloc(16 + (i * 7 & 0x1ff));
    }
    let window = start.elapsed().as_nanos() as f64 / N as f64;
    for p in ring {
        unsafe { heap::free(p) };
    }

    let start = Instant::now();
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            std::thread::spawn(move || {
                let mut rng = Rng(t + 11);
                let mut ring = vec![std::ptr::null_mut(); 4096];
                for _ in 0..N / 4 {
                    let at = rng.below(ring.len());
                    unsafe { heap::free(ring[at]) };
                    ring[at] = heap::alloc(rng.below(1024));
                }
                for p in ring {
                    unsafe { heap::free(p) };
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let churn = start.elapsed().as_nanos() as f64 / N as f64;
    println!(
        "BENCH lifo {lifo:.1} ns/pair, window {window:.1} ns/pair, 4-thread churn {churn:.1} ns/op"
    );
}
