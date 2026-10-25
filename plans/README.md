# SlopOS Plans

This directory holds only live proposals and open work, written statelessly
against the current tree: a plan describes what to build and why, never what
was already built or how the plan evolved. When part of a plan lands, remove
that part and re-baseline the rest; when nothing remains, delete the plan.
Git history preserves the record. Stable public documentation lives in the
sibling docs repo.

Even live plans can carry stale paths after refactors — verify paths before
editing.

## Current Files

| Document | Scope |
|----------|-------|
| `KNOWN_ISSUES.md` | Working notes on open issues; verify before using as source of truth |
| `ci-latency.md` | Where CI's wall clock goes, the lane rule that bounds it, and the cold-build work still open |
| `ui-overhaul.md` | The desktop's look, words and toolkit, its text and theme landed: semantic components so apps cannot look bad, a permission dialog that names the app and says what happens, polished window chrome, lock and login screens, one voice and glossary, and `just ui-shots` with a layout audit to see the result |

The driver-framework base has **landed and its plan is retired**. One `Bus` trait
(`drivers/src/driver_core/bus.rs`) and one generic `probe_bus` matchmaker drive the
PCI (`.driver_registry`) and platform/ACPI (`.platform_driver_registry`) registries, and
USB (`.usb_driver_registry`) offers each function through the same trait; each
keeps its own `#[repr(C)]` entry type and enumerator, and shares the binding protocol,
the devres claim table and `BoundDevice<B>`. Every device driver binds declaratively —
`boot_init!` carries no device drivers. Read the code rather than a document:
`driver_core::bus` for the model, `drivers/src/pci.rs`, `drivers/src/platform_bus/` and
`drivers/src/usb/bus.rs` for the three instances, and `drivers/src/tests/bus_generic.rs` for what the protocol guarantees.
Deferred-probe-to-fixpoint, and unbind and hotplug of PCI and platform devices, were the
plan's Phase 2 and are deliberately not planned in the mid term. USB built unbind for its
devices alone, on the Binding-above-Devres slot order and a `ClaimTable` release; unbind of
PCI and platform devices would build on the same two, and deferred probe on the
`Deferred` outcome.

USB has **landed and its plan is retired**. The xHCI driver runs every controller it can
take from the firmware, enumerates devices through hubs, binds `UsbBus` drivers for
keyboards and pointers, sticks and CDC Ethernet adapters, and removes devices cleanly.
Read the code rather than a document: `drivers/src/usb` for the driver and the tree,
`usb-core` and `hid-core` for the host-tested data and state machines, and `just test-usb`
for what QEMU grades. `AGENTS.md` states what it guarantees.

Self-hosting and bare metal have **landed and their plans are retired**. The
guest builds, installs and boots the whole system from a root that carries its
own toolchain, under QEMU and on the laptop, which SlopOS shares with another OS
through the installer, a crash partition and A/B slots, over NVMe, ext4 and the
RTL8168h. Read `AGENTS.md` rather than a document: it states the toolchain, the
recipes, the boot chain, the installer and the remote control, and `just
test-selfhost`, `just test-install`, `just test-installer` and `just
test-remote` grade them. The toolchain stays LLVM's: Cranelift and wild were
measured against this kernel and do not reach it, and
`scripts/gates/{codegen,linker}/` re-ask on every install. What remains open is
in `KNOWN_ISSUES.md`.

Persistent storage has **landed and its plan is retired**. A file written on one
boot is readable on the next on the root filesystem and under failure: a
writable disk is `/` by default, a jbd2 journal makes an
operation retractable and a crash recoverable, a writeback pass is bounded so
`sync(2)` no longer stalls every path walk on the mount for its duration, and a
per-process `ResourceKind::DiskBlocks` bounds what one principal can hold.
Read the code rather than a document: `fs/src/ext2/journal.rs` and
`ext4-core/src/{jbd2,recovery}.rs` for the journal's format and its replay, `fs/src/ext2/cache.rs` for how a commit, a rollback and
an eviction interact with it, `Ext2Fs::sync_step` for the bounded pass, and
`slopos-ostd/src/process/quota/disk.rs` for the block ledger. `AGENTS.md`
states the invariants a change there must keep.

The authority model has **landed and its plan is retired**. Authority is a flat
per-capability mask, the only authority a task carries — `task.flags` say where and how
it runs, never what it may do — and its classification is total by compile-time construction:
`define_syscall!` takes a mandatory `cap(X)` clause and emits it into the dispatch table
through the handler, so a `const` histogram in `core/src/syscall/handlers.rs` asserts both
totality over every registered entry point and each capability's recorded entry-point
count. Read the code
rather than a document: `slopos_ostd::authority` for the vocabulary and the witness,
`Endowment::check` in `core/src/exec/mod.rs` for the one place authority enters a child,
`slopos_ostd::seat` for the display and input seats, and `verification/proofs/authority.rs`
for the four machine-checked obligations. `scripts/check_authority_reachability.sh` is what
catches an unprivileged syscall reaching a power primitive two calls away, which a
slot-level gate cannot see. A mechanism's authority comes from `privd`, below.

The access model has **landed and its plan is retired**. Two principals carry
Unix identity: uid 0 for init, privd and the mechanisms privd runs, uid 1000
for the session. The VFS enforces owner, group and mode against the `Subject`
every entry takes, with `DacOverride` the only bypass. Authority reaches a
child only by delegation at spawn, a subset of the spawner's mask, and exec
keeps none of it. `/sbin/privd` runs a named action's mechanism for whoever the
sealed policy allows, judged by the identity the kernel captured when the
requester connected, and asks the person at the machine through a dialog only
the compositor can draw. Read the code rather than a document:
`slopos-ostd/src/process/cred.rs` and `fs/src/vfs/perm.rs` for credentials and
DAC, `slopos-ostd/src/process/identity.rs` for peer and program identity,
`priv-core` and `assets/privd/actions` for the policy, `userland/src/apps/privd/`
for the broker, and `userland/src/apps/init/service.rs` for who starts holding
what. `AGENTS.md`'s authority discipline states the invariants.

The live userland has **landed and its plan is retired**. Only a kernel change
needs a reboot: one sealed base on the boot partition, shared by the kernel
slots, is replaced and rolled back while the system runs, with the previous kept;
kernel and base each name the interface they serve or need; `libc.so` and a
versioned compositor protocol are the stable interfaces for the apps outside the
base; init supervises its services, restarts what a base switch changed and
re-executes itself; and the install medium opens a rescue shell that repairs an
installed disk. Read `AGENTS.md` rather than a document: its paragraphs on the
base, init, privd and the medium state the invariants, and `just test-install`,
`just test-installer` and `just test-remote` grade them.

System extensions have **landed and their plan is retired**. System software
outside the base ships as a sealed ext4 image followed by a verity tree, which
the kernel serves read-only at `/opt/<name>` and attests as it does base files;
the owner grants each one's services, privd actions, commands and users once,
by consent or by trusting a signing key under a capability ceiling, and
`extctl` updates and rolls it back live. Read `AGENTS.md` rather than a
document: its paragraphs on extensions state the invariants, and
`extension-core`, `fs/src/extension/` and `userland/src/apps/init/extensions.rs`
hold the code.

Users and login have **landed and their plan is retired**: a sealed user
database with an `admin` group and Argon2id passwords, privd's `user`, `admin`
and `greeter` principals with confirm, password and alone levels, a login
screen and console login, sessions that switch with the seat and the
clipboard following the one in front, `priv` in the `run0` model, and
`/usr/local` shared from the second person on with `~/.local` per person, a
one-person install unchanged. Read `AGENTS.md` rather than a document: its
paragraphs on the machine's people, sessions, privd and `priv` state the
invariants, and `accounts-core`, `userland/src/apps/init/session.rs` and
`userland/src/apps/priv.rs` hold the code.

The graphical installer has **landed and its plan is retired**. A live boot's
desktop opens `/bin/installer-ui --welcome`, an unprivileged appkit window
over `installer-core`'s `Wizard` that asks privd once for `installer --serve`
and speaks its typed protocol: Welcome, Disk, How, Review, Install, then Done
or Error, with before and after partition bars, a tagged action list, a root
size slider and a review guard worded by the damage. The engine owns every
proposal and checks the disk again before it writes. Read `AGENTS.md` rather
than a document: its installer paragraph states the invariants,
`installer-core` and `userland/src/apps/installer{,_ui}/` hold the code, and
`installer_ui_test` and `just test-installer` grade it.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
