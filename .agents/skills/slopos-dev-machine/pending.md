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
- `cargo`, `host_cargo` and `ensure_toolchain` resolve by `os()`: on Linux
  and macOS `cargo +slopos`, `cargo +<channel>` and
  `scripts/ensure_toolchain.sh`; on SlopOS the installed cargo and no
  toolchain step (build_userland.sh's std stamp is the check). `xtask` is
  `host_cargo run -p slopos-xtask` on both, built natively, so a build on
  SlopOS is held to the same ELF and `programs` gates as on the host. On
  SlopOS the base's recipe programs come from `/usr/local`, and a paired
  machine's base keeps its pairing.
- `just precommit` is the lane a commit made on SlopOS passes, the same
  command on the host: `fmt`, `test-host` (its Go half on the host only; on
  SlopOS with `TMPDIR=/var/tmp`, since `/tmp` is memory and some tests write
  disk images), `clippy`, `build dev`, `check-gates` (base interface, `xtask
  lint`, the dev kernel's ELF gates) and `libc-exports`. A root keeps 16G free
  (`ROOT_FREE_FLOOR`, was 6G) for the builds and the lane.
  `check-framekernel-gates` is
  `check-gates` plus the host's toolchain, pin and image-tool gates, and
  refuses on SlopOS. `install` runs `base-interface` and `libc-exports`, and
  `install-kernel` `base-interface`, before writing anything.
  `test-selfhost`'s guest runs `just precommit` after its two builds
  (`selfhost_test`'s `guest_grades_its_build`); the host's ELF gates on the
  exported kernels only confirm it. Pages: `self-hosting/development-machine`,
  `getting-started/build-test-recipes`, `development/adding-tests`.
- Tests run `just` in the clone as the session with the shell's `PATH`
  (`DEFAULT_PATH`), no `LD_LIBRARY_PATH` and `CARGO_HOME` in the clone's
  `builddir/cargo-home` (`slopos_userland::selfhost::just`). A person's
  `just` keeps their own `CARGO_HOME` (`~/.cargo`), where `selfhost.sh`
  defaulted to the clone's.
- `just` is a program of the toolchain (`toolchain/recipes/just/`, the `cargo`
  recipe template); `toolchain_test`'s rung `just_runs_a_justfile` runs a
  justfile and has the clone's `just boot` refuse.
