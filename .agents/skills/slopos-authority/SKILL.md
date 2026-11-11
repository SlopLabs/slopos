---
name: slopos-authority
description: >
  The capability mask as SlopOS's only authority, peer identity on AF_UNIX
  sockets, privd with its policy and consent dialog, `priv` and `endtask`.
  Use when a task mentions capabilities, `cap(...)`, `caps_drop`, `AT_SECURE`,
  `SO_PEERCRED`/`SO_PEERPROGRAM`, `slopos-ostd/src/authority`, `priv-core/`,
  `assets/privd/actions`, `userland/src/apps/privd`, `elevate`, an action,
  principal, mechanism, requester or consent question.
---

## Read first

- `docs/architecture/authority`: which capabilities exist, who holds them, how they pass
- `docs/architecture/privd`: how privd identifies, judges, asks and runs a request
- `docs/abi/privd-actions`: every action, the policy grammar, refusals, limits, wire frames
- `docs/abi/interfaces`: the consent channel and privd frames across a base switch

## Rules a change must keep

- Every check reads the task's capability mask. A check on `task.flags`, a
  path, a task id or a uid where a capability fits is a second authority.
- A spawn delegates only a subset of the spawner's mask; nothing about a
  program's path, owner or mode adds a capability.
- `exec` keeps only the universal capabilities. The one exception is init
  re-executing `/sbin/init` (`exec_keeps_authority`); to hand a mask on, spawn
  through `slopos_userland::held`.
- uid 0 alone is no principal. `root` is uid 0 running an attested base program.
- privd reads the requester from the socket's recorded identity, again before
  the verdict, after the question and before the start. Never look it up by
  pid or path.
- Every failure in privd is a refusal: a failed lookup, a bad frame, a
  requester that exited or changed program.
- A mechanism inherits nothing but the listed environment, directory and
  three descriptors, and acts only for the action `PRIV_ACTION` names.
- `describe`, `detail`, `verb` and every refusal sentence pass
  `slopos_priv_core::words`; a change to what a question may carry bumps
  `consent::VERSION`.
- `REQUESTER_CEILING`, `DELEGABLE` and `PRIVD_CAPS` are stated once in
  `priv-core/src/lib.rs`; a new capability needs a decision for each.
- A new privileged job is a new action with the narrowest shapes and
  capabilities, whose program calls `elevate`.
- A utest runs as uid 0 with every capability unless it declares
  `caps = …, uid = …`; a refusal test must declare them.

## Checks

- `just test-host`: priv-core's policy, words, consent and wire tests
- `just test 'slopos_core::utests::utest_privd'`: privd end to end, `endtask`
- `just test 'slopos_core::utests::utest_priv'`: `priv`, its ceiling, kept answers
- `just test 'slopos_core::utests::utest_peer'`: the peer socket options
- `just test 'slopos_core::utests::utest_spawn_privilege*'`: delegation between real processes
- `just check-framekernel-gates`: includes the `authority` ELF reachability gate

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
