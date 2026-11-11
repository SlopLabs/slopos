---
name: slopos-dev-machine
description: >
  The development machine: `just boot`'s persistent root, how the toolchain,
  the apps and the clone land on it, the git road between host and guest, and
  building the whole tree in the guest. Use when a task mentions
  `scripts/build_fs_image.sh`, `fs_tree.py`, `gen_verity.py`,
  `stage_workspace.sh`, `stage_ladder_fixtures.sh`, `selfhost.sh`,
  `scripts/lib/base.sh`, `qemu_run.sh`'s git or ssh peer, host or seed trees,
  the self-hosting root, `toolchain_test`, `selfhost_test`, `test-toolchain`,
  `test-selfhost`, `test-install-guest` or `bench-selfhost`.
---

## Read first

- `docs/self-hosting/development-machine`: the root, the git road, the checks
- `docs/self-hosting/toolchain`: what `just toolchain` builds and installs
- `docs/self-hosting/updating`: slots, bases, what `selfhost.sh install` drives
- `docs/architecture/filesystem`: checksummed images and the ownership map

## Rules a change must keep

- Trust nothing on a root image: read it only through `fs_tree.py`'s bound
  `ls -l` listings, and refuse an install before writing when the guest left
  something the manifest does not name.
- A host tree's manifest and a seal's record live in `<image>.host/`, on the
  host. The image's copies are the guest's to rewrite; never treat them as
  authority.
- A tree's owner comes from the ownership map and the image's owners record,
  never from the host directory's owner or modes.
- A seed tree is copied only onto a root without its directory. Never refresh
  `/src`: it is the guest's work.
- Blocks the guest owns stay un-attested through a grow and a refresh; a
  trailer counts only past the filesystem's extent.
- Keep both pins on each git daemon (`--interpolated-path`, and
  `--strict-paths` with one directory). Keep sshd publickey-only behind its
  `ForceCommand`, logging with `-E`: stderr is the connection.
- Nothing listens on a host port for the guest; peers run per connection
  through `guestfwd`.
- `build_kernel.sh` stays POSIX sh on `/bin/sh` and the coreutils; host-only
  steps (`ensure_toolchain.sh`, `xtask elf`, `xtask programs`) stay in the
  justfile.
- What a base carries is `scripts/lib/base.sh` alone.
- Tests run git and `selfhost.sh` in the clone as the session
  (`slopos_userland::selfhost`): git refuses a clone another uid owns.
- `just test` never needs `just toolchain`: a toolchain rung passes with a
  note on a root that carries none.

## Checks

- `python3 scripts/fs_tree.py --self-test`: tree installs, manifests, refusals
- `python3 scripts/gen_verity.py --self-test`: seals, the record, guest-owned blocks
- `just test-toolchain`: manifest, vendored clone, the ladder, a clone across a power-off
- `just test-selfhost`: the guest's build of `HEAD` under the host's gates and suite
- `just test-install-guest`: a guest build installed into a slot and booted

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
