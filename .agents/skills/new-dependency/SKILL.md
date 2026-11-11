---
name: new-dependency
description: >
  Add or bump a crate, a vendored source, a recipe, a font or other
  third-party input, within the licensing rules. Use when `Cargo.toml`,
  `Cargo.lock`, `toolchain/recipes/`, `assets/fonts/`, `NOTICE.md` or
  `slibc/NOTICE` changes, or when code is taken from another project.
---

## Read first

- `docs/development/licensing`: which licences mix, and why
- `docs/self-hosting/toolchain`: vendoring, the offline build, recipes
- `docs/verification/toolchain-gates`: the gates that hold all of it

## Rules a change must keep

- SlopOS is `GPL-3.0-or-later`. No verbatim code or prose from a
  GPL-2.0-only source (Linux, seL4, `rust/kernel/**`) or a CDDL one
  (illumos). ABI numbers, layouts and register offsets are facts and free to
  take; cite the specification or documented behaviour, never the
  implementation file.
- `slibc/`, `slibc-core/` and `abi/` are `MIT OR Apache-2.0` and stay
  permissive: no GPL or LGPL code, no dependency on a GPL crate, this tree's
  own included. A third-party crate there takes a `slibc/NOTICE` entry.
- A font or the CA bundle loads at runtime as its own file, licence text
  beside it. Never `include_bytes!` one into a shipped binary; the sites in
  `font/src/` stay `#[cfg(test)]`.
- Third-party code linked into a shipped binary takes a `NOTICE.md` entry;
  an `MIT OR Apache-2.0` crate elects MIT there.
- A build reads no registry: a new crate is a `Cargo.lock` diff, a `just
  vendor` and the `NOTICE.md` entry, in one commit.
- A kernel crate carries `#![forbid(unsafe_code)]` and declares no `alloc`
  dependency. A crate under `apps/` depends on `slopos-appkit`,
  `slopos-editor-core` and `slopos-image` alone.
- A recipe pins an `https` tarball by SHA-256 and builds it as shipped. Its
  patch only teaches the target (see `toolchain/AGENTS.md`); a GPL-2.0-only
  or CDDL recipe carries no patch.
- Git links no OpenSSL: nothing GPL-2.0-only may gain an Apache-2.0-only
  library in its `DT_NEEDED` closure.

## Checks

- `scripts/check_offline_build.sh --pins-only`: lockfiles against the
  vendored copies
- `just check-offline-build`: both targets from an empty `CARGO_HOME`
- `scripts/check_libc_license.sh`: every crate the C library resolves
- `scripts/check_recipes.sh`: recipe shape, patch grammar, licence closure
- `cargo run -p slopos-xtask -- lint alloc-dep app-deps unsafe-outside-ostd`

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
