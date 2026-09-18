//! `crt0` — the process entry object every slibc-linked program starts at.
//!
//! This is the whole of the C runtime's entry glue: the kernel enters at
//! `_start` with `rsp` pointing at the System V initial stack block (`argc`,
//! then `argv`, `envp` and the auxv), and `_start` hands that pointer to
//! [`__slibc_start`], which is slibc's `slibc/src/crt/mod.rs` entry that sets
//! up `environ`, TLS and stdio before calling `main`.
//!
//! The object is emitted with `--emit=obj`, not linked, so this crate has no
//! `#[panic_handler]` and needs none: there is no Rust code here, only the
//! `global_asm!` below. A `staticlib` crate type would require a panic runtime
//! whose `rust_begin_unwind` then duplicates the linked binary's own.

#![no_std]

// `_start` is `.text._start` so `userland/userland.ld`'s `*(.text .text.*)`
// picks it up, and the explicit `.note.GNU-stack` keeps the linker from
// inferring an executable stack from a section-less hand-written object.
//
// The sequence is the System V `_start -> __libc_start_main` contract:
//   * `xor rbp, rbp` — the ABI's "outermost frame" marker.
//   * `mov rdi, rsp` — argument 1 is the *unaligned* initial stack pointer,
//     because `__slibc_start` reads `argc` from `[rdi]`.
//   * `and rsp, -16` — align before the call, so the callee sees
//     `rsp % 16 == 8` after the pushed return address, as the ABI requires.
//   * `ud2` — `__slibc_start` is `-> !`; returning is a bug, not a fallthrough.
core::arch::global_asm!(
    ".section .text._start,\"ax\",@progbits",
    ".p2align 4",
    ".globl _start",
    ".type _start, @function",
    "_start:",
    "xor rbp, rbp",
    "mov rdi, rsp",
    "and rsp, -16",
    "call __slibc_start",
    "ud2",
    ".size _start, . - _start",
    ".section .note.GNU-stack,\"\",@progbits",
);
