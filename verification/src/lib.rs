//! SlopOS OSTD critical-path verification crate.
//!
//! The proofs are **not** part of this Rust library target: they live as
//! standalone Verus files under `proofs/`, `use vstd::prelude::*` (which
//! resolves only under the pinned Verus toolchain, not the kernel's nightly),
//! and are checked by `just verify`. This `lib.rs` exists only so
//! `verification/` is a first-class workspace member. Per-module status is in
//! `STATUS.md`; `README.md` covers running and upgrading the verifier.

#![no_std]
