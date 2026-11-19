# Pending for the docs

- `docs/architecture/base`, `docs/abi/interfaces`, `docs/self-hosting/updating`:
  before v1 `BASE_INTERFACE_MIN` is `BASE_INTERFACE`, so a kernel serves the
  base interface of its own tree alone and `info` prints `Base interfaces:
  <n>-<n>`. A change to the private call surface still takes the next number
  and a ledger row, but weighs no older base: a call may change shape or be
  removed, and a retired call keeps no slot. A kernel and a base either side
  of such a change go in as a pair, the base first, which waits on the disk,
  then the kernel into the spare slot. The range a kernel serves, its note
  and `bootctl`'s refusals stay as they are, for v1 to widen the range again.
- `docs/verification/toolchain-gates`, `docs/abi/interfaces`: the two contract
  gates are xtask modules, `scripts/check_base_interface.sh`,
  `scripts/check_libc_exports.sh` and their Python helpers are gone, and both
  run on SlopOS. `just base-interface [--record | --print]` (`xtask
  base-interface`) reads slopos-abi's rustdoc JSON as before, and also the
  Linux-numbered calls the kernel answers: `static SYSCALL_TABLE` in
  `core/src/syscall/handlers.rs`, one `linux <n> <SYSCALL_NAME>` line each,
  with findings `unresolved`, `range` (a number at or above
  `SYSCALL_PRIVATE_BASE`), `meta` and a floor of 100. A call the kernel
  starts or stops answering takes the next number; that took interface 13.
  `just libc-exports [--record]` (`xtask libc-exports <libc.so>`) reads
  `.dynsym` with `object` rather than `llvm-readobj`, and canonicalises the
  dump of the clang `scripts/cxx_host_tools.sh` resolves (`$CLANG`) in Rust;
  neither half is ever skipped. Their fixtures are `cargo test -p
  slopos-xtask`. `just install` runs both before writing anything,
  `install-kernel` the first. Pages naming the old scripts too: `abi/syscalls`,
  `architecture/{base,software,syscalls,userland}`,
  `verification/tcb-discipline`, and `development/adding-a-syscall`, whose
  `--record` steps become `just base-interface --record` / `just libc-exports
  --record`, and which gains the step that registering a Linux call in
  `SYSCALL_TABLE` takes the next `BASE_INTERFACE`.
- `docs/abi/syscalls`, `docs/abi/interfaces`, `docs/architecture/syscalls`:
  interface 14 removes seven private calls and renumbers the rest densely
  from `SYSCALL_PRIVATE_BASE` (`sys_info` is 0, `display_stats` 46; 47 in
  all): `klog_write` (a write to `/dev/kmsg`), `sigdefault` (`rt_sigaction`
  per signal, the set blocked around the loop), `keymap_get_name` (init's
  `/run/keymap`), `clipboard_copy`/`clipboard_paste` (the compositor's
  clipboard alone), `set_display_mode` and `roulette_draw`. `/dev/kmsg`'s
  write is in the surface as `slopos_abi::fs::KMSG_*` (path, the 1024-byte
  record bound, 10 records per process each 5000 ms); `CLIPBOARD_MAX_SIZE`
  and the `SESSION_LOGINS` root are gone. slibc's `syslog` writes the node.
- `tools/initramfs` takes the base's recipe programs by their base path
  (`RECIPE_PROGRAMS`, `sbin/<name>` or `bin/<name>`), each read by name from
  `RECIPE_PROGRAM_DIR`, and their licence texts from
  `RECIPE_LICENSE_DIR/<recipe>`; `RECIPE_PREFIX` is gone. The justfile names
  the recipes' prefix's `libexec` and `share/licenses` on the host and
  `/opt/slopos-toolchain`'s on SlopOS.
- `docs/architecture/software`: the apps under `apps/` ship as the image
  `slopos-essentials` (five commands, four apps). `build_userland.sh` stages
  `builddir/apps` with `.extension/{release,declare}`, its version a UTC
  stamp that moves only when the staged files do
  (`builddir/slopos-essentials.version`); `_build-userland` packs it through
  `build_extension.sh` into `builddir/slopos-essentials/`. Entries name
  `/opt/slopos-essentials/bin/<app>`; the editor's manual is looked for in
  `~/.local/share`, `/opt/slopos-essentials/share`, `/usr/share`. Every root
  is offered the image, no root carries the `apps` tree, init no longer lays
  the medium's apps out on a live boot, and `just install` installs the
  image with `extctl install`, which an administrator approves.
- Phase 6 of `installed-software.md`: `/usr/local` is gone, with
  `tree-install`, `tree-core`, `userland/src/tree.rs`, tree manifests in the
  guest (`/var/lib/slopos/trees`; `fs_tree.py` keeps only the host's beside
  the image, and lost `walk`) and `utest_tree_install`. `DEFAULT_PATH` is
  `/bin:/sbin:/opt/bin`, a person's `PATH` `/bin:/sbin:~/.local/bin:/opt/bin`.
  A bare library name: `SEARCH_DIRS`, the program's image's `lib`, then
  `$HOME/.local/lib` for a process neither `AT_SECURE` nor uid 0's. The dock
  reads each image's share, `/usr/share`, `~/.local/share`. The shell's
  rules for a command, with no `SHELL_COMPLETION_PATH`, come from the
  `share/shell/completions` of the image `/opt/bin/<command>` links into, then
  the base's. Software for everyone is an image; a person's own build is a
  file in `~/.local`, copied there. Pages: `architecture/software` (trees,
  manifests, who writes it, the hand-over, its tests and files go),
  `architecture/base`, `abi/interfaces` (the dock's, the shell's and the
  tree manifest's rows, `DEFAULT_PATH`), `abi/glossary` (Tree and Host tree
  go; App, Owners record), `architecture/overview`, `architecture/filesystem`,
  `architecture/userland`, `architecture/processes`,
  `status/current-capabilities`, `status/known-limitations`.
- `docs/abi/interfaces`, `docs/abi/syscalls`: interface 15 drops
  `slopos_abi::extension::MAX_ACTIVATIONS`; `ext_activate`'s `EBUSY` is the
  resident activations passing their budget, an eighth of usable memory.
