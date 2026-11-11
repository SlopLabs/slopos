---
name: slopos-toolchain
description: >
  The toolchain that builds SlopOS and the one cross-built to run on it: the
  owned sysroot, the rustc and LLVM forks, the C++ runtime, the recipes and
  the offline build. Use when a task touches `toolchain/`, `targets/`,
  `scripts/make_*.sh`, `scripts/bootstrap_slopos_toolchain.sh`,
  `scripts/build_recipes.sh`, or mentions sysroot, build-std, PIN, bootstrap,
  libc++, clang, recipe, vendor or PGO.
---

## Read first

- `docs/self-hosting/toolchain`: how every piece is built, pinned and why
- `docs/verification/toolchain-gates`: what each `scripts/check_*.sh` gate holds
- `docs/self-hosting/development-machine`: how the guest uses the toolchain

## Rules a change must keep

- Never write inside the rustup toolchain directory. Forks are patch files
  under `toolchain/`; the trees under `third_party/` are remade from them.
- Editing a patch means updating its `patch_sha256` in the PIN that lists it
  (`toolchain/PIN`, `toolchain/compiler/PIN`, `toolchain/cxx/PIN`).
- The built-in target in `toolchain/compiler/0001` and
  `targets/x86_64-unknown-slopos.json` must stay equal.
- The LLVM port lives twice (`toolchain/llvm/`, `toolchain/llvm-rustc/`), cut
  against different LLVM versions. A port change goes into both.
- Take the C++ ABI flag from `make_slopos_cxx.sh --print-abi-flags`; never
  restate it. Choose host LLVM tools only through `scripts/cxx_host_tools.sh`.
- Keep libc++ and libc++abi in one `libc++.so`: two copies of the exception
  machinery in a process make an exception uncatchable.
- A recipe patch only teaches upstream that SlopOS exists. Anything else a
  port needs is a slibc or kernel fix. A recipe under GPL-2.0-only or CDDL
  carries no patch.
- Git and what it loads link no OpenSSL: curl takes TLS from Mbed TLS.
- A new dependency is a `Cargo.lock` diff, `just vendor` and a `NOTICE.md`
  entry. Do not fold `.cargo/vendor.toml` into `.cargo/config.toml`.
- Settings that change the compiler's code go in
  `scripts/lib/rustc_build_settings.sh`, shared by `just toolchain` and
  `just toolchain-profile`, or a PGO profile stops fitting. Bump
  `PROFILE_FLOW` when the profile script's steps or workload change.
- System programs keep `SYSTEM_RUSTFLAGS` (`rust-lld`, `panic=abort`);
  `userland/userland.ld` discards `.eh_frame` on that assumption.

## Checks

- `just check-framekernel-gates`: every pin and toolchain gate, cheap halves
- `just check-rustc-target`: built-in target against the JSON (needs `just rustc-src`)
- `just check-bootstrap-config`: cross-build plan, wrapper compile and link
- `just check-llvm-port`, `just check-clang-driver`: the port (need `just llvm-src`)
- `scripts/check_recipes.sh`: recipe patch shape, argument grammar, licences
- `just check-offline-build`: kernel and userland from vendored crates only
- `just test-toolchain`: the ladder, on a built toolchain

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
