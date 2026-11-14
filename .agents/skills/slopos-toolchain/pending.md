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
