//! `Pod` impls for `slopos-abi` types.
//!
//! They live here rather than in `slopos-abi` because OSTD owns the trait and
//! all of the kernel's `unsafe`.

use slopos_abi::damage::DamageRect;
use slopos_abi::fs::UserFsEntry;
use slopos_abi::net::SockAddrIn;
use slopos_abi::unix::SockAddrUn;

use crate::Pod;

// SAFETY: `SockAddrIn` is `#[repr(C)]` over `u16 + u16 + [u8; 4]
// + [u8; 8]` with no padding (16 bytes total, asserted in the abi
// crate). All field types are primitive integers / byte arrays; every
// byte pattern represents a valid value (`Copy` already derived).
unsafe impl Pod for SockAddrIn {}

// SAFETY: `SockAddrUn` is `#[repr(C)]` over `u16 + [u8; UNIX_PATH_MAX]`
// with no padding (110 bytes total, asserted in the abi crate). All
// field types are primitive integers / byte arrays; every byte pattern
// represents a valid value (`Copy` already derived).
unsafe impl Pod for SockAddrUn {}

// SAFETY: `UserFsEntry` is `#[repr(C)]` over `[u8; 256] + u8 + [u8; 7] + u64`.
// Every hole is a named `_pad` field, so the type has no implicit padding at
// all (272 bytes, asserted in the abi crate) and no byte of it can carry
// uninitialized kernel memory. All field types are primitive integers / byte
// arrays; every byte pattern represents a valid value (`Copy` already
// derived).
unsafe impl Pod for UserFsEntry {}

// SAFETY: `DamageRect` is `#[repr(C)]` over `i32 × 4` with no padding.
// All field types are primitive integers; every byte pattern represents
// a valid value (`Copy` already derived).
unsafe impl Pod for DamageRect {}
