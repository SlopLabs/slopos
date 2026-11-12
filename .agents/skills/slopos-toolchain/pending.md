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
