---
name: slopos-base-software
description: >
  The base (the read-only userland every kernel boots), its live generations
  and interface number, and how software outside it is found: images under
  /opt and a person's ~/.local. Use when a task touches fs/src/basefs,
  boot/src/base.rs, boot-core base or newc, abi base_interface,
  tools/initramfs, root_layout, app_entry, apps/, or mentions base_activate,
  BASE_INTERFACE, libc.so exports, app entries, search paths or ~/.local.
---

## Read first

- `docs/architecture/base`: what a base holds, how one is chosen and switched
- `docs/architecture/software`: where installed software lives, the apps
- `docs/abi/interfaces`: each interface between separately built parts, and its check
- `docs/verification/toolchain-gates`: the base-interface and libc-exports gates

## Rules a change must keep

- Any change to a private syscall, or a structure or constant it takes, or to
  which Linux calls the kernel answers, takes the next `BASE_INTERFACE` and a
  ledger row. Before v1 a kernel serves that number alone (`BASE_INTERFACE_MIN`
  follows it), so a call may change or go.
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
- Software for everyone is an image; no directory a person can write is on
  anyone's default search path, for commands, libraries, app entries or
  completions.
- Nothing with power searches what a person can write: the loader skips
  `~/.local/lib` for `AT_SECURE` or uid 0, and `priv` takes a program only
  from a directory only uid 0 can change.

## Checks

- `just test-host`: `boot-core` and the packer's host tests
- `just base-interface`: the private surface against its number
- `just libc-exports`: exports and layouts only grew
- `cargo run -p slopos-xtask -- lint app-deps`: what a crate under `apps/` depends on
- `just test-install`: base choice at boot, install, live switch and rollback

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
