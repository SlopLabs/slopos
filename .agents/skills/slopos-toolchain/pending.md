# Not yet in the docs

- Recipe template `cargo` (`scripts/build_recipes.sh`): a crates.io `.crate`
  built by `cargo +slopos` for the target JSON, std from the owned sysroot,
  linked (and a build script's C compiled) by the SlopOS compiler wrapper,
  `--locked` to the `lock=` file beside the recipe. A package that lock takes
  with no source is the libc fork or a `toolchain/crates/` port of that name
  and version, staged beside the build and patched in for crates.io; the
  recipe's stamp adds the std stamp and the ports. `program`s are
  `bin/<binary>`. First user: just 1.59.0, which `just toolchain` installs at
  `bin/just`. Page: `self-hosting/toolchain` (recipes, the port table).
- Porting just grew the libc fork and slibc: Linux's whole `_SC_*` and `_PC_*`
  tables (slibc's `sysconf`/`pathconf` still answer only what they know, the
  rest `EINVAL`), the declarations of `getpwnam_r`, `getgrgid_r`,
  `getgrnam_r`, `getgrouplist` and `initgroups`, and `mkfifoat`.
  `toolchain/crates/` gains `nix 0.31.3`, the one just's lock takes.
- The std fork names the target in `std::env::consts` (`OS` `slopos`,
  `FAMILY` `unix`, `.so` libraries): before, both were empty on SlopOS, and
  just's `os()`, which the justfile resolves recipes by, read nothing.
- The root workspace excludes `builddir/`, where a `cargo` recipe unpacks its
  crate.
- The cross-built toolchain carries rustdoc, rustfmt (`cargo fmt`) and clippy
  (`cargo clippy`): `build.tools` in `bootstrap_slopos_toolchain.sh` names
  them, and `check_bootstrap_config.sh` holds the plan to building rustdoc
  and installing rustfmt and clippy for SlopOS. rustdoc is what the
  base-interface gate reads the abi through; `toolchain_test` gains
  `rustfmt_formats_a_crate` and `clippy_lints_a_crate`, and `toolchain_starts`
  asks every new program its version. Pages: `self-hosting/toolchain` (the
  components).
- The std fork's `env::current_exe()` on SlopOS reads `AT_EXECFN`, the
  absolute path the kernel opened, through slibc's `getauxval`; it answered
  `Unsupported` before, and `cargo-fmt` and `cargo-clippy` find `rustfmt` and
  `clippy-driver` beside themselves through it.
- The stamp `bootstrap_slopos_toolchain.sh` leaves in `src/llvm-project`
  covers `rustc_src_sha256` as well as `toolchain/llvm-rustc/`: a channel
  bump that leaves the port's patch untouched still re-extracts rustc's
  bundled LLVM and rebuilds it. Before, such a bump built the new compiler
  against the previous nightly's LLVM. Page: `self-hosting/toolchain` (what
  each tree records).
- A channel bump moves, besides the patches the pin gate names: the libc
  fork's version to the one `library/Cargo.lock` takes, with
  `toolchain/recipes/just/Cargo.lock` and `NOTICE.md` after it; the crate
  ports to the versions `cargo tree` resolves for a SlopOS host; and
  `third_party/vendor` and the built recipes, which the gates ask to be
  remade. Page: `verification/toolchain-gates` (Re-record).
- `just toolchain` ends in the image `slopos-toolchain`:
  `builddir/slopos-toolchain/install` is laid out as `/opt/slopos-toolchain`
  shows it and `scripts/build_extension.sh` packs it into
  `builddir/slopos-toolchain/image/slopos-toolchain.{ext,meta}`; a toolchain
  the image checks refuse leaves no image. `--stage <dir>` copies the image
  directory.
- The recipes are configured for `GUEST_PREFIX=/opt/slopos-toolchain`; every
  template puts configuration in `share/etc` and system programs in `libexec/`
  (e2fsprogs, `sshd`), since an image holds only `bin`, `libexec`, `lib` and
  `share`. x.py's `sysconfdir` is `share/etc`.
- The image carries no `libc.so` or `libc.a`: a program links the running
  base's `libc.so`. Headers are `share/include`. The clang config makes
  `share` the sysroot, so the driver's own `<sysroot>/include` and
  `<sysroot>/include/c++/v1` name them and `-nostdinc++`/`-nostdlibinc` still
  drop them; `-B<CFGDIR>/../lib` finds `crt0.o` and `libbuiltins.a`, and
  `-L/lib -L<CFGDIR>/../lib` search the base's `/lib` first.
- No link in the image climbs: `bin/ld.lld` and `bin/llvm-ar` are copies, and
  a recipe's climbing link becomes a copy of what it reaches or a link to a
  sibling holding that copy (git's names in `bin/`).
- The image ships the empty SafeStack archive at
  `lib/rustlib/x86_64-slos/lib/librustc-nightly_rt.safestack.a`;
  `build_kernel.sh` writes one only where none is.
- The toolchain's identity (`tp_toolchain_identity`,
  `scripts/lib/toolchain_identity.sh`) hashes `toolchain/PIN`, the std and
  libc forks, `compiler/`, `crates/`, `llvm/`, `llvm-rustc/`, `cxx/`,
  `recipes/` and `rustc_build_settings.sh`; it is the image's release
  `display`, and its
  `version` the build time. `build_userland.sh` and, on SlopOS, `just setup`
  hold an image sysroot to it and name the one the tree needs.
- `check_bootstrap_config.sh` resolves `libc.so` to `builddir/libc.so`, the
  base's, not the install's.
