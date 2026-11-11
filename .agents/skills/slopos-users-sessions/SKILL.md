---
name: slopos-users-sessions
description: >
  Users, file permissions, accounts, init's service supervision and login
  sessions in SlopOS. Use when a task touches `Cred`, `Subject`, `DacOverride`,
  uids, `ROOT_LAYOUT`, the owners or accounts record, `accounts-core/`,
  `/bin/users`, `authcheck`, `userland/src/apps/init/`, `/run/init`, login
  tags, seats, `/bin/session`, `/bin/login`, the login or lock screen, or
  per-person quota.
---

## Read first

- `docs/architecture/users`: ids, permission checks, the ownership map, accounts, passwords
- `docs/architecture/init`: what init starts, restarts, `/run/init`, re-execution
- `docs/architecture/sessions`: the session table, login tags, seats, login, lock

## Rules a change must keep

- uid 0 is an identity, never a power: gate on a capability (`DacOverride`,
  `SetId`, `ProcSignal`), never on `uid == 0`. No set-ID bit is honoured.
- Every VFS entry takes the `Subject` it acts for; nothing below the syscall
  layer reads the current task. Kernel callers pass `Subject::KERNEL`.
- Change `ROOT_LAYOUT` and `PERSON_TREES` together with
  `scripts/lib/root_layout.sh` and `tools/initramfs`; a test holds them equal.
- An owners record that is absent or unreadable grants nothing. Never fall
  back to the one-person record for a root that already exists.
- The accounts record and `/etc/passwd`, `group`, `shadow` are believed only
  sealed. Writers hold `ACCOUNTS_STORE.lock` across reading the record and
  writing the database, and write `/etc/shadow` first, from the record alone.
- Only `/sbin/authcheck` reads a hash back. A secret travels on a pipe or a
  terminal with echo off (`userland/src/secret.rs`), never in argv.
- A pty handed to a mechanism or a login shell comes from
  `process::openpty_handed_on` and is never unlocked for opens by path.
- A login tag is set only at spawn, by a `SetId` holder, and is never given
  twice in a boot. Init ends a session by its tag, not by its process tree.
- Init is one thread in one `poll`: after the boot jobs, nothing blocks.
- `/run/init` judges by `SO_PEERCRED` plus the peer's base program as of the
  request; a new request states who may send it in `judge`.
- Only init moves the seats (`seat_switch`). A compositor reads init's word
  before each pass's input, and a desktop behind counts as locked at once.
- A re-exec state change an older init would misread takes a new
  `slopos-init-state` header number.

## Checks

- `just test '*utest_dac*'`: permission checks and the ownership map
- `just test '*utest_accounts*'`: `/bin/users`, passwords, the sealed record
- `just test '*utest_supervision*'`: restarts, re-execution, ending by tag
- `just test '*utest_sessions*'`: login, switching, locking, console, quota
- `just test-host`: `accounts-core`, `priv-core`, the map's three copies
- `just check-fs-image --map`: a disk image against the map and record

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
