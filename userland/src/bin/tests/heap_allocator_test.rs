use slopos_userland as _;

use slopos_slibc::alloc::RawBuffer;

fn test_simd_fill_survives_demand_fault() -> bool {
    // A non-zero `[u32; 3]` fill compiles to an AVX `vmovups` memset, and on
    // fresh pages it faults mid-SIMD: a kernel that loses user vector state
    // across a page fault leaves stale zeros behind. Each ~3.6 MB round forces
    // never-faulted pages.
    const FILL: [u32; 3] = [0x20, 0xe6e6e6, 0x1e1e1e];
    for _ in 0..20 {
        let mut v: Vec<[u32; 3]> = Vec::new();
        v.resize(300_000, FILL);
        if v.iter().any(|e| *e != FILL) {
            return false;
        }
        drop(v);
    }
    true
}

fn test_alloc_dealloc_basic() -> bool {
    let Some(mut buf) = RawBuffer::new(64) else {
        return false;
    };
    buf.fill_with(|i| (i as u8).wrapping_mul(3));
    buf.verify(|i| (i as u8).wrapping_mul(3))
}

fn test_forward_coalesce() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(b);
    drop(a);

    let Some(c) = RawBuffer::new(128) else {
        return false;
    };
    drop(c);
    true
}

fn test_backward_coalesce() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(a);
    drop(b);

    let Some(c) = RawBuffer::new(128) else {
        return false;
    };
    drop(c);
    true
}

fn test_format_pattern_stability() -> bool {
    let mut seed: u32 = 0x4D59_5DF4;

    for iter in 0..1000usize {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let size = 32 + (seed as usize % 97);

        let Some(mut buf) = RawBuffer::new(size) else {
            return false;
        };

        let base = (iter as u8).wrapping_mul(13);
        buf.fill_with(|i| base.wrapping_add(i as u8));
        if !buf.verify(|i| base.wrapping_add(i as u8)) {
            return false;
        }
    }

    let Some(mut big) = RawBuffer::new(64 * 1024) else {
        return false;
    };
    big.write_byte(0, 0xA5);
    big.write_byte(64 * 1024 - 1, 0x5A);
    big.read_byte(0) == 0xA5 && big.read_byte(64 * 1024 - 1) == 0x5A
}

fn test_mmap_fallback() -> bool {
    let size = 256 * 1024;
    let Some(mut buf) = RawBuffer::new(size) else {
        return false;
    };
    buf.write_byte(0, 0xC1);
    buf.write_byte(size - 1, 0x1C);
    buf.read_byte(0) == 0xC1 && buf.read_byte(size - 1) == 0x1C
}

fn test_realloc_grow() -> bool {
    let Some(mut p) = RawBuffer::new(32) else {
        return false;
    };
    p.fill_with(|i| i as u8);

    let Some(p) = p.realloc(128) else {
        return false;
    };
    for i in 0..32 {
        if p.read_byte(i) != i as u8 {
            return false;
        }
    }

    let Some(p) = p.realloc(256) else {
        return false;
    };
    for i in 0..32 {
        if p.read_byte(i) != i as u8 {
            return false;
        }
    }
    true
}

fn test_small_recycling() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    drop(a);

    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(b);
    true
}

fn test_mass_free_then_realloc() -> bool {
    // Catches bookkeeping that survives a mass free: the batch coalesces back
    // into segment-spanning chunks before the fresh allocations below.
    let mut batch = Vec::new();
    for round in 0..16usize {
        let Some(mut buf) = RawBuffer::new(32 * 1024) else {
            return false;
        };
        let tag = round as u8;
        buf.fill_with(|i| tag.wrapping_add(i as u8));
        batch.push(buf);
    }
    for (round, buf) in batch.iter().enumerate() {
        let tag = round as u8;
        if !buf.verify(|i| tag.wrapping_add(i as u8)) {
            return false;
        }
    }
    drop(batch);

    for round in 0..32usize {
        let Some(mut buf) = RawBuffer::new(4 * 1024) else {
            return false;
        };
        let tag = (round as u8).wrapping_mul(31);
        buf.fill_with(|i| tag.wrapping_add(i as u8));
        if !buf.verify(|i| tag.wrapping_add(i as u8)) {
            return false;
        }
    }
    true
}

fn test_segment_release() -> bool {
    use slopos_slibc::mem::malloc::heap_stats;

    // A batch larger than one segment forces the arena to grow; freeing it all
    // must munmap the extra segments, one default-sized one staying resident.
    let before = heap_stats();
    let mut batch = Vec::new();
    for round in 0..64usize {
        let Some(mut buf) = RawBuffer::new(32 * 1024) else {
            return false;
        };
        let tag = round as u8;
        buf.fill_with(|i| tag.wrapping_mul(7).wrapping_add(i as u8));
        batch.push(buf);
    }
    let peak = heap_stats();
    if peak.arena_size <= before.arena_size {
        return false;
    }

    drop(batch);
    let after = heap_stats();
    after.arena_size < peak.arena_size
}

fn test_free_chunks_are_reused_before_the_arena_grows() -> bool {
    use slopos_slibc::mem::malloc::heap_stats;

    const BIG: usize = 48 * 1024;
    const REQUEST: usize = 40 * 1024;

    // Keepers between the chunks stop any two frees from coalescing.
    let mut keepers = Vec::new();
    let mut bigs = Vec::new();
    for _ in 0..4 {
        let (Some(big), Some(keeper)) = (RawBuffer::new(BIG), RawBuffer::new(64)) else {
            return false;
        };
        bigs.push(big);
        keepers.push(keeper);
    }
    let mut smalls = Vec::new();
    for _ in 0..64 {
        let (Some(small), Some(keeper)) = (RawBuffer::new(512), RawBuffer::new(64)) else {
            return false;
        };
        smalls.push(small);
        keepers.push(keeper);
    }
    // Nothing free may fit the request but the big chunks about to be freed.
    let mut plugs = Vec::new();
    while heap_stats().largest_free >= REQUEST {
        let Some(plug) = RawBuffer::new(REQUEST - 4 * 1024) else {
            return false;
        };
        plugs.push(plug);
    }

    // Freed last, the small chunks are what a bounded sort of the unsorted
    // list reaches first; the big ones sit past it.
    drop(bigs);
    drop(smalls);
    let before = heap_stats().arena_size;
    let Some(served) = RawBuffer::new(REQUEST) else {
        return false;
    };
    let grew = heap_stats().arena_size != before;
    drop(served);
    drop(plugs);
    drop(keepers);
    !grew
}

fn test_direct_registry() -> bool {
    use slopos_slibc::mem::malloc::heap_stats;

    // Allocations past the threshold get a dedicated mapping tracked in the
    // direct registry; the count must follow create and free. Past the most a
    // freed mapping can raise the threshold to, so no earlier test's frees
    // move it into the arena.
    let base = heap_stats().direct_count;
    let size = 40 * 1024 * 1024;
    let Some(mut buf) = RawBuffer::new(size) else {
        return false;
    };
    buf.write_byte(0, 0xAB);
    buf.write_byte(size - 1, 0xCD);
    if heap_stats().direct_count != base + 1 {
        return false;
    }
    if buf.read_byte(0) != 0xAB || buf.read_byte(size - 1) != 0xCD {
        return false;
    }
    drop(buf);
    heap_stats().direct_count == base
}

/// A child forked while another thread is inside the allocator can allocate.
fn test_fork_while_another_thread_allocates() -> bool {
    fork_during(
        |n| {
            let v: Vec<u8> = Vec::with_capacity(64 + n % 1024);
            core::hint::black_box(&v);
        },
        || {
            let v = vec![7u8; 4096];
            v[4095] == 7
        },
    )
}

/// A child forked while another thread is inside the loader can walk the
/// loaded objects, as its unwinder does.
fn test_fork_while_another_thread_holds_the_loader() -> bool {
    unsafe extern "C" fn visit(
        _: *mut slopos_slibc::ld_so::api::DlPhdrInfo,
        _: usize,
        _: *mut core::ffi::c_void,
    ) -> i32 {
        0
    }
    fork_during(
        |_| {
            let loader = slopos_slibc::ld_so::lock();
            spin(2000);
            drop(loader);
            spin(200);
        },
        || unsafe {
            slopos_slibc::ld_so::api::dl_iterate_phdr(Some(visit), core::ptr::null_mut()) == 0
        },
    )
}

/// A child forked while another thread is starting threads can start one.
fn test_fork_while_another_thread_starts_threads() -> bool {
    fork_during(
        |_| {
            let _ = std::thread::spawn(|| {}).join();
        },
        || std::thread::spawn(|| {}).join().is_ok(),
    )
}

/// A child forked while another thread flushes every stream can flush them.
fn test_fork_while_another_thread_flushes_streams() -> bool {
    fork_during(
        |_| unsafe {
            slopos_slibc::stdio::file::fflush(core::ptr::null_mut());
        },
        || unsafe { slopos_slibc::stdio::file::fflush(core::ptr::null_mut()) == 0 },
    )
}

fn spin(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

/// Fork repeatedly while another thread runs `churn` in a loop, and require
/// every child to pass `child`.
fn fork_during(churn: fn(usize), child: fn() -> bool) -> bool {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const FORKS: usize = 64;
    let stop = Arc::new(AtomicBool::new(false));
    let churner = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0usize;
            while !stop.load(Ordering::Relaxed) {
                churn(n);
                n = n.wrapping_add(1);
            }
        })
    };
    let ok = (0..FORKS).all(|_| fork_and_run(child));
    stop.store(true, Ordering::Relaxed);
    churner.join().is_ok() && ok
}

/// `free` leaves errno alone however contended the allocator is: the lock's
/// futex calls report through errno, and C code reads it after a free.
fn test_contended_free_preserves_errno() -> bool {
    use slopos_slibc::{errno_get, errno_set};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SENTINEL: i32 = 0x5105;
    let stop = Arc::new(AtomicBool::new(false));
    let churners: Vec<_> = (0..3)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    core::hint::black_box(Vec::<u8>::with_capacity(512));
                }
            })
        })
        .collect();
    let mut kept = true;
    for _ in 0..20_000 {
        let p = slopos_slibc::alloc(64);
        errno_set(SENTINEL);
        slopos_slibc::dealloc(p);
        if errno_get() != SENTINEL {
            kept = false;
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    churners.into_iter().all(|h| h.join().is_ok()) && kept
}

/// Fork a child that runs `child` and exits, and reap it within a deadline; a
/// child stuck on a lock it inherited held is killed and counts as failure.
fn fork_and_run(child: fn() -> bool) -> bool {
    use slopos_slibc::process::wait::WNOHANG;
    use slopos_slibc::process::{WEXITSTATUS, WIFEXITED, shim, waitpid};

    let pid = shim::fork();
    if pid == 0 {
        shim::_exit(i32::from(!child()));
    }
    if pid < 0 {
        return false;
    }
    let mut status = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match unsafe { waitpid(pid, &mut status, WNOHANG) } {
            0 => std::thread::sleep(std::time::Duration::from_millis(1)),
            reaped if reaped == pid => return WIFEXITED(status) && WEXITSTATUS(status) == 0,
            _ => return false,
        }
    }
    unsafe {
        slopos_slibc::signal::kill(pid, 9);
        waitpid(pid, &mut status, 0);
    }
    false
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("alloc_dealloc_basic", test_alloc_dealloc_basic),
        ("forward_coalesce", test_forward_coalesce),
        ("backward_coalesce", test_backward_coalesce),
        ("format_pattern_stability", test_format_pattern_stability),
        ("mmap_fallback", test_mmap_fallback),
        ("realloc_grow", test_realloc_grow),
        ("small_recycling", test_small_recycling),
        ("mass_free_then_realloc", test_mass_free_then_realloc),
        ("segment_release", test_segment_release),
        (
            "free_chunks_are_reused_before_the_arena_grows",
            test_free_chunks_are_reused_before_the_arena_grows,
        ),
        ("direct_registry", test_direct_registry),
        (
            "simd_fill_survives_demand_fault",
            test_simd_fill_survives_demand_fault,
        ),
        (
            "fork_while_another_thread_allocates",
            test_fork_while_another_thread_allocates,
        ),
        (
            "fork_while_another_thread_holds_the_loader",
            test_fork_while_another_thread_holds_the_loader,
        ),
        (
            "fork_while_another_thread_starts_threads",
            test_fork_while_another_thread_starts_threads,
        ),
        (
            "fork_while_another_thread_flushes_streams",
            test_fork_while_another_thread_flushes_streams,
        ),
        (
            "contended_free_preserves_errno",
            test_contended_free_preserves_errno,
        ),
    ]);
}
