---
name: slopos-boot-install
description: >
  How SlopOS boots, is installed on a disk and updates itself: the disk
  layout, the A/B kernel slots and two bases, the installer, the install
  medium, the rescue shell, the crash record and the boot screen.
  Use when a task mentions boot-core, fat-core, installer-core, splash-core,
  fate-core, tools/bootdisk, bootctl, installer, installer-ui, roulette,
  limine.conf, slots, the ESP, the payload, `root=`, `panic=reboot`,
  scripts/selfhost.sh, build_iso.sh or build_bootdisk.sh.
---

## Read first

- `docs/architecture/boot`: boot stages, root selection, boot screen, crash record rules
- `docs/self-hosting/installing`: the installer, the medium and payload, the rescue shell
- `docs/self-hosting/updating`: bootctl, slots, bases, loader variables, safe writes
- `docs/architecture/base`: what a base is and how a kernel chooses one
- `docs/development/diagnostics`: reading a panic and its crash record

## Rules a change must keep

- State the disk layout, GPT, loader variables and `limine.conf` only in
  `boot-core`; the kernel, `bootctl`, the installer and `tools/bootdisk` read
  it from there.
- Never write the ESP after install. The default slot is `LoaderEntryDefault`;
  `limine.conf` names no `default_entry`.
- On a shared ESP write nothing outside `\EFI\SlopOS\`. Keep SlopOS's own
  partition type GUIDs, never the Discoverable Partitions ones.
- `bootctl` never writes the default slot's kernel, and refuses an install
  that leaves the default, running or armed kernel serving neither base.
- Replace files through `fat-core` copy-on-write: new clusters flushed first,
  then one directory-entry store.
- `previous.img` holds the base that last ran: move current there only when
  current is the running base.
- The installer engine prints nothing and installs only the proposal it made
  last, after re-reading the disk. Put no write before that check.
- `installer-ui` holds no capability; only `installer --serve` under
  `org.slopos.install` touches a disk. Never offer a read-only disk or the
  one carrying the medium's GPT disk GUID.
- Installer words come from `theme-core::words`; sizes are decimal.
- The panic path only writes: no lock waited on, no allocation, the header
  chunk last. `bootctl collect` erases a record only once its copy is durable.
- `splash-core` and `fate-core` use integers only (the kernel is soft-float).
- `root=auto` means the first disk that is not removable.

## Checks

- `just test-host`: boot-core, fat-core, installer-core, splash-core, fate-core
- `just test-install`: slots, one-time boot, commit, panic fallback, bases
- `just test-installer`: the installer on every disk shape, rescue, reinstall
- `just boot-log`: the live ISO reaches `/sbin/init`

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
