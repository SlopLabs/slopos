---
name: slopos-base-software
description: >
  The base (the read-only userland every kernel boots), its live generations
  and interface number, and the software installed outside it in trees.
  Use when a task touches fs/src/basefs, boot/src/base.rs, boot-core base or
  newc, abi base_interface, tools/initramfs, tree-core, userland tree.rs,
  tree_install, root_layout, app_entry, apps/, or mentions base_activate,
  BASE_INTERFACE, libc.so exports, app entries, /usr/local or ~/.local.
---

## Read first

- `docs/architecture/base`: what a base holds, how one is chosen and switched
- `docs/architecture/software`: trees, manifests, who writes `/usr/local`, the apps
- `docs/abi/interfaces`: each interface between separately built parts, and its check
- `docs/verification/toolchain-gates`: the base-interface and libc-exports gates

## Rules a change must keep

- Any change to a private syscall, or a structure or constant it takes, takes
  the next `BASE_INTERFACE` and a ledger row. Raise `BASE_INTERFACE_MIN` only
  when an older base could no longer run; a retired call keeps its slot and
  answers `ENOSYS`.
- `libc.so` only grows: no export removed, no `slibc/include` layout changed.
- A program outside the base is dynamic, needs `libc.so` and holds no
  `syscall` instruction. A crate under `apps/` depends only on what
  `xtask lint app-deps` allows.
- Change what a valid base is only in `boot-core::base::check`, and the
  archive reader only in `boot-core::newc`, never in a caller.
- Everything in `base_activate` that can fail runs before the switch; the
  base mounts move together under the mount table's lock, all or none.
- A generation's lifetime is read from the open-file, page-set and process
  tables, not counted. Code keeping a base inode past one syscall must show
  in one of them (`basefs::Hold`). The booted generation is never freed.
- Believe a tree manifest only if uid 0 or the installing uid wrote it and it
  parses; anything else is an unfinished install of the new tree.
- uid 0 installs a tree only from the copy `tree::stage` makes, never from
  the tree in place: its owner can swap a file for a link while it is read.
- Nothing with power searches what a person can write: the loader skips
  `~/.local/lib` and `/usr/local/lib` for `AT_SECURE` or uid 0, and `priv`
  takes `/usr/local/bin` only when `root_layout::settled`.
- The second `users add` hands `/usr/local` to uid 0 first, or adds nobody.

## Checks

- `just test-host`: `boot-core`, `tree-core` and the packer's host tests
- `scripts/check_base_interface.sh`: the private surface against its number
- `scripts/check_libc_exports.sh builddir/libc.so`: exports and layouts only grew
- `cargo run -p slopos-xtask -- lint app-deps`: what a crate under `apps/` depends on
- `just test-install`: base choice at boot, install, live switch and rollback

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
