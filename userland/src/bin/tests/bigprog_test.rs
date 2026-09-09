#![feature(restricted_std)]

//! A toolchain-sized process, in the dimensions that used to cap one.
//!
//! Every case here was a refusal before: the binary is 24x the 1 MiB the slab
//! would hand out for the old whole-file `exec` buffer, the anonymous mapping
//! 4x the old `Pages` quota, the file mapping reaches past a non-page-aligned
//! EOF `mmap` refused outright, and the stack and the `fork` each pass a
//! constant that used to be the whole of one.
//!
//! `BLOB` is what makes the ELF large, so its bytes are checked at both ends
//! and every megabyte between: a streamer that mis-computed a file offset would
//! load a binary that still runs and has the wrong bytes in it.

use slopos_abi::syscall::posix::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use slopos_userland as _;
use slopos_userland::syscall::{core as sys_core, fs, memory, process};

const PAGE: u64 = 4096;

/// A prime chunk length, repeated: the whole 24 MiB cannot be const-evaluated
/// byte by byte, and a period coprime with every alignment is what makes an
/// offset the streamer got wrong detectable.
const CHUNK_LEN: usize = 65_521;
const CHUNKS: usize = 384;
const BLOB_LEN: usize = CHUNK_LEN * CHUNKS;

/// ~24 MiB of `.rodata`: the file's size, and therefore the size `exec` has to
/// stream. Not zero, or it would land in `.bss` and cost the file nothing.
#[used]
static BLOB: [[u8; CHUNK_LEN]; CHUNKS] = [build_chunk(); CHUNKS];

const fn build_chunk() -> [u8; CHUNK_LEN] {
    let mut out = [0u8; CHUNK_LEN];
    let mut i = 0usize;
    while i < CHUNK_LEN {
        out[i] = (i % 251) as u8;
        i += 1;
    }
    out
}

const fn blob_expected(index: usize) -> u8 {
    ((index % CHUNK_LEN) % 251) as u8
}

#[inline(never)]
fn blob_at(index: usize) -> u8 {
    // Volatile, or the compiler answers from the initialiser rather than the
    // loaded image.
    unsafe { core::ptr::read_volatile(BLOB.as_ptr().cast::<u8>().add(index)) }
}

fn mmap_failed(v: u64) -> bool {
    v == 0 || (v as i64) < 0
}

/// The bytes `exec` streamed are the bytes the file holds, at the start, at the
/// end, and at every megabyte between.
fn test_large_image_loaded_intact() -> bool {
    let mut offset = 0usize;
    while offset < BLOB_LEN {
        if blob_at(offset) != blob_expected(offset) {
            return false;
        }
        offset += 1024 * 1024;
    }
    blob_at(BLOB_LEN - 1) == blob_expected(BLOB_LEN - 1)
}

/// 1 GiB of anonymous address space, touched sparsely. The old `Pages` quota
/// refused past 256 MiB, and an eager `mmap` would have wanted every frame.
fn test_gigabyte_of_anonymous_memory() -> bool {
    const LEN: u64 = 1024 * 1024 * 1024;
    const STRIDE: u64 = 64 * 1024 * 1024;

    let base = memory::mmap(
        0,
        LEN,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if mmap_failed(base) {
        return false;
    }

    let mut off = 0u64;
    while off < LEN {
        unsafe {
            core::ptr::write_volatile((base + off) as *mut u64, base ^ off);
        }
        off += STRIDE;
    }

    let mut off = 0u64;
    let mut ok = true;
    while off < LEN {
        let got = unsafe { core::ptr::read_volatile((base + off) as *const u64) };
        if got != (base ^ off) {
            ok = false;
            break;
        }
        off += STRIDE;
    }

    memory::munmap(base, LEN);
    ok
}

/// A mapping reaching past a non-page-aligned EOF: the tail is zero-filled
/// rather than refused.
fn test_file_mapping_past_a_ragged_eof() -> bool {
    use slopos_abi::fs::O_RDONLY;

    const PATH: &core::ffi::CStr = c"/var/bigprog.dat";
    const BODY: &[u8] = b"phase one";

    if fs::write_durable(PATH, BODY).is_err() {
        return false;
    }
    let Ok(fd) = fs::open_cstr(PATH, O_RDONLY) else {
        return false;
    };

    let base = memory::mmap(0, PAGE, PROT_READ, MAP_PRIVATE, fd.raw() as i64, 0);
    if mmap_failed(base) {
        let _ = fs::close_fd(fd);
        return false;
    }

    let mut ok = true;
    for (i, want) in BODY.iter().enumerate() {
        let got = unsafe { core::ptr::read_volatile((base as *const u8).add(i)) };
        if got != *want {
            ok = false;
        }
    }
    for i in BODY.len()..PAGE as usize {
        if unsafe { core::ptr::read_volatile((base as *const u8).add(i)) } != 0 {
            ok = false;
            break;
        }
    }

    memory::munmap(base, PAGE);
    let _ = fs::close_fd(fd);
    ok
}

/// Recurse past the old fixed 1 MiB stack. Every slot is written volatile, or
/// SROA splits the array into scalars and the frame this case is about
/// disappears into a 24-byte prologue.
#[inline(never)]
fn burn_stack(depth: u32) -> u64 {
    let mut frame = [0u64; 512];
    for (i, slot) in frame.iter_mut().enumerate() {
        unsafe { core::ptr::write_volatile(slot, depth as u64 ^ i as u64) };
    }
    let deeper = if depth == 0 { 0 } else { burn_stack(depth - 1) };
    unsafe { core::ptr::read_volatile(&frame[0]) + core::ptr::read_volatile(&frame[511]) + deeper }
}

fn test_stack_grows_past_a_megabyte() -> bool {
    // 512 u64s plus the call overhead is a little over 4 KiB per frame, so 768
    // frames is ~3 MiB — past the 1 MiB that used to be the whole stack.
    const DEPTH: u32 = 768;
    let mut want = 0u64;
    for d in 0..=DEPTH as u64 {
        want += (d ^ 0) + (d ^ 511);
    }
    burn_stack(DEPTH) == want
}

/// `fork` of a large resident set used to panic: the parent's PTE snapshot was
/// one `KVec` of 24-byte entries, so ~170 MiB in a single VMA passed the slab's
/// 1 MiB ceiling and the `.expect()` took the machine down. `LEN` is past that.
fn test_fork_with_a_large_resident_set() -> bool {
    const LEN: u64 = 192 * 1024 * 1024;

    let base = memory::mmap(
        0,
        LEN,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if mmap_failed(base) {
        return false;
    }
    let mut off = 0u64;
    while off < LEN {
        unsafe {
            core::ptr::write_volatile((base + off) as *mut u64, off);
        }
        off += PAGE;
    }

    let pid = process::fork();
    if pid == 0 {
        let seen = unsafe { core::ptr::read_volatile((base + LEN - PAGE) as *const u64) };
        sys_core::exit_with_code(if seen == LEN - PAGE { 0 } else { 1 });
    }
    if pid < 0 {
        memory::munmap(base, LEN);
        return false;
    }

    let status = process::waitpid(pid as u32);
    memory::munmap(base, LEN);
    status == 0
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("large_image_loaded_intact", test_large_image_loaded_intact),
    (
        "gigabyte_of_anonymous_memory",
        test_gigabyte_of_anonymous_memory,
    ),
    (
        "file_mapping_past_a_ragged_eof",
        test_file_mapping_past_a_ragged_eof,
    ),
    (
        "stack_grows_past_a_megabyte",
        test_stack_grows_past_a_megabyte,
    ),
    (
        "fork_with_a_large_resident_set",
        test_fork_with_a_large_resident_set,
    ),
];

fn main() {
    slopos_slibc::test_harness::run_with_progress("bigprog", CASES);
}
