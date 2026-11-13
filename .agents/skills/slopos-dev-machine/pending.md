# Not yet in the docs

- `scripts/selfhost.sh` is gone: the justfile is the one build interface on
  both systems. In the guest, `just build [VARIANT]`, `just install [VARIANT]`
  and `just install-kernel [VARIANT]` replace `selfhost.sh build|install|
  install-kernel`; `just boot` prints that loop. `build` makes the kernel and
  the base it boots with (no fs image any more); its default variant follows
  `KERNEL_RELEASE` (dev unless 1) on both systems, while `install` and
  `install-kernel` default to `release`. Pages: `self-hosting/development-machine`,
  `self-hosting/updating`, `getting-started/quickstart`, `architecture/base`,
  `architecture/software`, `development/writing-an-app`.
- A recipe says where it runs. A build or a gate runs on both systems; one that
  emulates a machine or needs the host's image or cross-build tools depends on
  `_host` first and refuses on SlopOS in one line; `install` and
  `install-kernel` depend on `_slopos` and refuse elsewhere, naming
  `just remote-install`. Host-only steps carry the guard.
- `cargo`, `host_cargo`, `xtask`, `ensure_toolchain` and `gate` resolve by
  `os()`: on Linux and macOS `cargo +slopos`, `cargo +<channel>`,
  `scripts/ensure_toolchain.sh` and xtask; on SlopOS the installed cargo, no
  toolchain step (build_userland.sh's std stamp is the check) and no gate
  (the host grades a guest build, `just test-selfhost`). On SlopOS the base's
  recipe programs come from `/usr/local`, and a paired machine's base keeps its
  pairing.
- Tests run `just` in the clone as the session with the shell's `PATH`
  (`DEFAULT_PATH`), no `LD_LIBRARY_PATH` and `CARGO_HOME` in the clone's
  `builddir/cargo-home` (`slopos_userland::selfhost::just`). A person's
  `just` keeps their own `CARGO_HOME` (`~/.cargo`), where `selfhost.sh`
  defaulted to the clone's.
- `just` is a program of the toolchain (`toolchain/recipes/just/`, the `cargo`
  recipe template); `toolchain_test`'s rung `just_runs_a_justfile` runs a
  justfile and has the clone's `just boot` refuse.
