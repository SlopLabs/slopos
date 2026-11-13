# SlopOS

An x86-64 operating system in Rust: a framekernel, its own C library, toolchain port, desktop and installer. Pre-alpha.

This file is the rules. The long form is the docs, and a skill routes you to the pages for what you touch: read it before changing a subsystem.

## Layout

- Kernel: `kernel/`, `slopos-ostd/` (the trusted core), `boot/`, `core/`, `mm/`, `sched/`, `fs/`, `drivers/`, `net/`, `video/`. A `*-core` crate is I/O-free and host-tested.
- Userland: `userland/` (the base: init, privd, compositor, tools), `apps/` (outside the base), `appkit/`, `windowing/`, `slop-protocol/`, `slibc/`, `slibc-core/`, `abi/`.
- `toolchain/` (forks as patches, recipes), `targets/`, `scripts/`, `scripts/gates/` (tracked gate data), `tools/` (`xtask` checks, `run_tests`), `verification/`, `plans/` (open work).
- `builddir/` is output and `third_party/` is fetched or generated: edit neither.
- `slopos-ostd/`, `apps/`, `toolchain/`, `tools/xtask/` and `scripts/gates/` have their own `AGENTS.md`: read it before editing there.

## Commands

`just --list` shows them all.

- `just setup`: the pinned nightly and the owned sysroot.
- `just build`, `just iso`: `builddir/kernel-dev.elf`, `builddir/slop.iso`.
- `just boot` (the persistent machine), `just boot-live`, `just boot-log` (headless; serial in `test_output.log`).
- `just test ['glob']`: the suite under QEMU. The glob is positional: `just test 'slopos_mm::*'`.
- `just test-host` (host crates, gate tests), `just check-framekernel-gates` (source and ELF gates).
- `just ci`: every CI lane, or one: `ci-gates`, `ci-candidates`, `ci-suite`, `ci-toolchain`, `check-miri`, `verify`.

## Rules

- **Unsafe.** `slopos-ostd` is the only kernel crate that may use `unsafe`; every other one carries `#![forbid(unsafe_code)]`. The only exempt sites are in `kernel/src/main.rs` and `boot/src/ffi_boundary.rs`.
- **Allocation.** Kernel code allocates through `slopos_ostd::mm::heap` (`KBox`, `KVec`, `KArc`, ...), never `alloc::*`. Build a large struct in place with `try_init`.
- **Stack and registers.** A kernel frame stays under 2 KiB, and the kernel is soft-float: no vector, x87 or MMX instruction.
- **Authority.** The per-task capability mask is the only authority. A check never reads `task.flags`, a path, a task id or a uid where a capability fits. A uid is an identity, never a power.
- **Tasks.** `KArc<Task>` is the only owning task handle and `TaskRef` the only way to hold one outside OSTD. A task exits only from its own context: kill is a flag, and blocked code returns `Err(WaitAbort::Killed)`.
- **Style.** `#![no_std]` nightly Rust, four-space indent, braces on the same line, `pub(crate)` helpers, cross-module APIs prefixed with their subsystem. Assembly is Intel syntax and documents its register contract.
- **Comments.** Default to none; let names, types and structure carry intent. Comment only what code cannot say (a hardware quirk, an ordering or locking requirement, a deliberate deviation), in one or two lines, the why. `# Safety` sections and `///` API docs are exempt.
- **A check is Rust.** A new check is a module of `tools/xtask` or a Go log gate with its fixtures as tests. A new `scripts/check_*.sh` is refused.
- **Gate data.** Never hand-edit `scripts/gates/**` to make a run pass. Re-measure, and say what added the delta.
- **Licensing.** `GPL-3.0-or-later`. No verbatim code or prose from a GPL-2.0-only or CDDL source. `slibc/`, `slibc-core/` and `abi/` stay `MIT OR Apache-2.0`, and hold no symlink. Fonts and the CA bundle load at runtime, never `include_bytes!`. Third-party code takes a `NOTICE.md` entry.
- **Interfaces only grow.** The private call surface is numbered by `slopos_abi::BASE_INTERFACE`, and a change takes the next number, the only one a kernel serves before v1. `libc.so`'s exports and layouts only grow. Syscall numbers hold Linux's x86-64 allocation.
- **Hardware.** LAPIC and IOAPIC are required (QEMU `q35`), and VirtIO needs MSI-X or MSI.

## Docs

The docs repository (`SlopLabs/slopos-docs`) is the long form. A page written `docs/<section>/<page>` is read, in order of preference, from:

1. `$SLOPOS_DOCS/content/docs/<section>/<page>.mdx`, where `SLOPOS_DOCS` defaults to `../slopos-docs`;
2. the `slopos-docs` MCP server (`.mcp.json`): `get_page` with `/docs/<section>/<page>`, or `search`;
3. `https://slopos.sloplabs.net/llms.mdx/<section>/<page>/content.md` (index at `/llms.txt`).

`SYNCED` there names the slopos commit the pages were checked against. The source wins. A change that adds or alters an invariant the docs do not state appends a note to its skill's `pending.md` in the same commit; skill `docs-sync` moves notes into pages. `xtask lint agents-doc` holds this file, the nested ones and the skills to their budgets: do not grow them.

## Skills

In `.agents/skills/<name>/SKILL.md`.

| Skill | Read it when the task touches |
|---|---|
| `slopos-toolchain` | `toolchain/`, the sysroot, the forks, recipes, vendoring, `--pgo` |
| `slopos-dev-machine` | `just boot`'s root, image builders, guest git, `just install` |
| `slopos-boot-install` | `boot-core`, `bootctl`, slots, the installer, rescue, crash records |
| `slopos-base-software` | the base, its interface, `basefs`, `/usr/local` trees, `tree-install` |
| `slopos-extensions` | `extctl`, `ext_activate`, extension images, services, grants |
| `slopos-authority` | capabilities, peer identity, privd and consent, `priv` |
| `slopos-users-sessions` | uids and file permissions, accounts, init, sessions, login |
| `slopos-storage` | the block layer, NVMe, ext4, the journal, verity, fs images |
| `slopos-usb-input` | xHCI, USB drivers, HID, keyboards, pointers, keymaps |
| `slopos-net` | NICs, the IP stack, `netctl`, TLS, `remoted`, `scripts/remote.py` |
| `slopos-desktop` | `theme-core`, appkit, apps, the compositor's chrome, icons |
| `slopos-windowing` | buffers, frames, damage, `slop-protocol`, `windowing/` |
| `slopos-settings` | the Settings app, `settings-core`, preferences |
| `slopos-memory-sched` | the commit ledger, OOM, allocation, cpufreq, task ownership |
| `slopos-gates` | `unsafe`, `tools/xtask`, ELF gates, clippy, proofs, Miri |
| `slopos-testing` | tests, `just test*`, log gates, boot options, kconsole, CI |
| `ratchet-remeasure` | a gate failing on a cap, allowlist or recorded list |
| `new-dependency` | a crate, recipe, font or other third-party input |
| `security-triage` | a milestone, a release or handoff, `CVSS.md` |
| `docs-sync` | syncing the docs, a full `pending.md` |
| `caveman-commit` | every commit message |

## Testing

Tests are `stest!` (kernel) and `utest!` (userland), run under QEMU. Verify with the narrowest run that exercises the change, then the lanes it touches. A green `just test` is not the bar: CI runs gates it does not.

## Pre-commit

1. `cargo fmt --all`, and stage what it reformatted. Never commit unformatted Rust.
2. `just ci` or the lanes the change touches; on SlopOS `just precommit`.
3. Write the message with skill `caveman-commit`, always; never by hand.
4. Commit directly to `develop`; no branch, PR or push unless asked.

Say in the message what moved a ratchet.

## Safety boundaries

Work only inside this repository and the docs repository. Never copy kernel binaries to system paths, install or chainload on real hardware, run outside QEMU/OVMF, or touch `/boot`, `/efi` or other host locations.

One exception: a SlopOS machine the user booted with a base paired to this host (`just remote-serve`) may be driven through `scripts/remote.py` alone. It runs programs, moves files, installs a base on the boot partition and a kernel into the spare slot. Never write its ESP, its firmware variables beyond what `bootctl` does, or another system's partitions. Commit a slot (`--commit`, `bootctl commit`) only once the boot it came from answered with the expected tag.

## Security triage

Mandatory after each major milestone and before any release or handoff: skill `security-triage`, over what recent commits touched. `CVSS.md` lists open findings only; a fix deletes its entry. Never score a speculative issue.
