# SlopOS Known Issues

---

## An install from the live ISO at 512 MiB aborts writing the slots

**Status**: Open
**Severity**: Medium (the default `just boot-live` machine cannot install)
**Component**: `userland/src/apps/installer/` (step 5), `mm/src/process_vm.rs:2709`

`ROULETTE=0 just iso`, booted as `boot-live` boots it (`QEMU_MEM=512M`) with
the ISO in the CD drive and a blank 32 GiB NVMe disk, erases and installs
through the window up to step 5, *writing both kernel slots and the base*;
the engine then dies of `SIGABRT` right after the kernel logs
`process_vm_mmap: Failed to insert VMA`, and the window says *the installer
stopped before it said how the install ended*. The base it writes is 27.5 MB.
Whether the VMA map's insert fails for memory or for another reason, and
whether more guest memory avoids it, is not known.

---

## A window in `__spawn_kernel_io` leaves a live thread preserved by nothing

**Status**: Open
**Severity**: Low (not currently reachable)
**Component**: `slopos-ostd/src/sync/kernel_io_task.rs:577-581`

`spawn_at_priority` publishes the task to the registry before `stop.bind_task`
binds its id. Between those two lines the thread is live but its id is not yet
on the stop, so it is preserved by neither `task_registry_reset` nor
`reset_preserving` — a test scope entered in that window would tear it down.

Not reachable today because all three kernel-I/O spawns happen on the BSP, but
nothing in the type system or the gates enforces that.

---

## A task on an AP can stall behind three unbounded or O(CPUs) scheduler waits

**Status**: Open
**Severity**: Low (latency only; no correctness consequence)
**Component**: `sched/src/scheduler.rs`, `sched/src/task/task_reclaim.rs`

Task termination itself does not stall an AP — `task_terminate` serialises with a
local `PreemptGuard` (`sched/src/task/task_lifecycle.rs:836`), and the AP pause is
reached only from `task_shutdown_all` (`:1177`), whose sole production caller is
`kernel_shutdown` (`boot/src/shutdown.rs:165`). Three other waits on the ordinary
scheduling path can, and a latency-sensitive task such as the compositor is where
they would be seen.

**The `on_cpu` handover spin** (`scheduler.rs:1286`) is the one genuinely
unbounded wait a dispatching AP can hit. Having dequeued a task whose prior CPU
has not finished its switch-out tail, the AP spins on that CPU's Release store of
`on_cpu` with no bound and no fallback. The window is short by construction — it
is the tail of a context switch — but a prior CPU that takes an interrupt inside
it extends the spin by exactly that handler's duration. The spin services
pending cross-CPU work (`sync::spin_relax`), so it is never half of a mutual
wait with a peer blocked on a shootdown this CPU owes; what remains is latency,
not a hang.

**`unschedule_task`'s per-CPU sweep** (`scheduler.rs:1017-1026`) takes every
CPU's scheduler in turn on every termination, to find the one queue the task
might be on. O(CPUs) lock acquisitions per termination, on a path a
spawn-heavy workload runs constantly.

**The task graveyard drains only from the idle dispatcher**
(`task_reclaim.rs:174-208`), so under sustained load dead tasks' kernel stacks and
address spaces accumulate until some CPU goes idle. The fix is a per-CPU deferred-work
list so reclaim stops depending on CPU 0 being idle; recorded here because it shares this
shape.

Fixes for the first two: bound the `on_cpu` spin, re-enqueueing rather than
spinning past a threshold; and record the owning CPU on the task so
`unschedule_task` takes one lock instead of `n`. Neither is scheduled work.

---

## A link larger than an eighth of memory dies on a refused file-page fault

**Status**: Open
**Severity**: Medium (the debug tests kernel does not link in a 4G guest)
**Component**: the file map's per-process pin share (`commit ledger` boot step,
`PinnedBytes`), `mm/src/page_fault.rs`

A clean file page a process has mapped stays pinned until it is unmapped, so
the file map's per-process share — an eighth of usable memory — bounds what one
process may map. The debug tests kernel's link exceeds 4 GiB's share:
`pinnedbytes` peaks at exactly the cap, `rust-lld` takes a refused file-page
fault and dies, and the filesystem it was writing remounts read-only. The
self-hosting checks and `just boot` run at 6G for that reason. Reclaiming clean
mapped pages under pressure lifts the bound; a refused fault in one process
should also stop failing a whole mount.

---

## Boot turns the scheduler off globally while APs already run user tasks

**Status**: Open
**Severity**: Low (a boot-time window; latency only)
**Component**: `sched/src/lifecycle.rs` (`boot_step_scheduler_init`),
`sched/src/scheduler.rs` (`resume_unswitched`)

`boot_step_scheduler_init` disables scheduling for every CPU until the BSP
enters its own loop, though the APs are already running user tasks.
`resume_unswitched` undoes the `Blocked` a `schedule()` that could not switch
leaves behind, so a waiting task spins through the window rather than hanging.
Turning the scheduler off per CPU rather than globally closes the window.

---

## An idle CPU's page zeroing can fault on `INVALID_PAGE_FRAME`

**Status**: Open (cause not found)
**Severity**: Medium (a kernel fault; no path a user drives is known to reach it)
**Component**: `mm/src/page_alloc/buddy.rs:1297-1352` (`prezero_idle`),
`mm/src/page_alloc/pcp.rs`

Under heavy host load an idle CPU's `prezero_idle` can take
`INVALID_PAGE_FRAME` from its per-CPU cache and fault zeroing the physical
address it names. Which path leaves that value where the cache hands out a
frame is not known.

---

## Installing for everyone through privd is bounded by half a ceiling and by what everyone may read

**Status**: Open
**Severity**: Low (limits, no wrong result)
**Component**: `userland/src/tree.rs:271-320` (`tree::stage`),
`userland/src/apps/tree_install.rs:54-65`

`org.slopos.software.install` copies the tree into
`/var/lib/slopos/tree-install` before installing it, and both count against the
mechanism's `DiskBlocks` ceiling, so a tree installed through privd is at most
half of it. The copy takes only what everyone may read, reached through
directories everyone may search, so once homes are 0700 a person installs for
everyone only from a tree outside their home.

---

## A trusted key whose ceiling words pass 320 bytes cannot be added from the session

**Status**: Open
**Severity**: Low (usability)
**Component**: `priv-core/src/consent.rs:28` (`MAX_SHOWN`),
`userland/src/apps/extctl/mod.rs` (`key add`)

privd asks about no command line past 320 escaped bytes, and `key add`'s line
names the key's fingerprint, the key itself and both its ceilings word by
word, so a key whose ceilings name most of the base's is refused when the
session asks; root and the paired agent, whom nothing asks, add it.

---

## Extension activations cost more than their images

**Status**: Open
**Severity**: Low (memory and boot time; only an `ExtensionUpdate` holder makes an activation)
**Component**: `fs/src/extension/index.rs:153`, `fs/src/extension/device.rs:139-150`,
`extension-core/src/admit.rs:186-225`, `userland/src/apps/extctl/mod.rs:962-978`

- An activation's index holds up to `MAX_ENTRIES` (65 536) names of up to 255
  bytes, about 17 MB of kernel heap, and 32 activations about 570 MB.
- Image blocks are cached twice: by the root's filesystem the verifying device
  reads through, and by the activation's own `Ext4Mount`.
- Each block that fails its tree logs `EXT: image block <n> fails its tree`,
  unthrottled.
- Every boot's `extctl activate-all` runs `admit::activation`, whose symbol
  check reads each extension's ELF files whole through the tree, before privd
  starts.

---

## A port held by an extension init cannot read stops another extension's service

**Status**: Open
**Severity**: Low (robustness)
**Component**: `userland/src/apps/init/extensions.rs:1349-1420` (`wanted`)

When init cannot read an extension's record or activation it leaves that
extension's services running, holding their sockets, and counts only their
descriptors. Another extension's service that listens on one of those ports is
not told apart from a free one: its bind fails, it spends its starts and gives
up until the unreadable extension reads again.

---

## A base switch during init's re-execution goes unacted on

**Status**: Open
**Severity**: Low (a narrow window; a switch takes `BaseUpdate`)
**Component**: `userland/src/base_watch.rs:31-40`, `userland/src/apps/init/reexec.rs`

The re-executed init opens a fresh `BaseWatch`, whose manifest is the base
current when it opens; the state handed across carries no manifest. A switch
that lands while init re-executes is never compared against the old manifest,
so privd and remoted are not restarted for it.

---

## Every base lookup takes the generation table's lock

**Status**: Open
**Severity**: Low (contention)
**Component**: `fs/src/basefs/mod.rs:234-252`

`BaseFs::generation` takes `BASEFS_GENERATIONS` to find the generation an
inode's high half names, on every lookup, read and stat of a base file, on
every CPU.

---

## The compositor's client tasks stream multishot polls

**Status**: Open
**Severity**: Low (one client's connection stalls)
**Component**: `userland/src/apps/compositor/mod.rs:1612-1640`

A multishot poll fires only when readiness changes, so a client message that
lands between a read and the ring's next pass wakes nothing until the client
sends again. The consent channel takes a oneshot poll each time for this
reason (`mod.rs:1513-1530`); the protocol clients stream.

---

## `recvmsg` on `AF_UNIX` takes every queued descriptor

**Status**: Open
**Severity**: Low (a sender loses only its own messages)
**Component**: `net/src/unix_socket/mod.rs:779-835`,
`userland/src/apps/privd/mod.rs:576-582`

One `recvmsg` drains every descriptor queued on the connection, drops those
past `SCM_MAX_FDS`, and hands one whose bytes arrive during the read to the
bytes that read took. privd, which holds a request to the number of
descriptors it names, refuses both of two requests a requester sends before
privd reads the first.

---

## privd's requesters: signals, input and draining

**Status**: Open
**Severity**: Low (robustness)
**Component**: `userland/src/privd.rs:786-797`, `userland/src/held.rs:46-50`,
`userland/src/apps/privd/mod.rs:697-706`, `userland/src/apps/remoted/serve.rs:142-213`

- A request privd reads as its drain begins is refused as restarting, or finds
  its connection closed, and no requester retries either.
- A held program (`priv`, the rescue shell) gets a signal sent to every process
  of its job twice: once from the sender, once passed on.
- Input and signals reach a mechanism by separate paths — bytes through the
  requester's relay, signals as frames privd passes on — so neither is ordered
  against the other, and input typed before an interrupt can arrive after it.
- A remote command's timeout kills its requester, but remoted then waits for
  end of file on the command's output, which a mechanism ignoring `SIGTERM`
  (`bootctl` while it writes, `extctl`) holds open past the timeout.

---

## A base install takes effect for every slot with no trial boot

**Status**: Open
**Severity**: Medium (a base that fails in userland stays current)
**Component**: `userland/src/apps/bootctl.rs:510` (`base_install`)

A base that passes `boot_core::base::check` and fails in userland stays
current for every slot until a reinstall that keeps the root writes the
medium's, or the medium's rescue shell rolls it back. A power cut between the
two base writes leaves one image twice, which the default kernel still boots.

---

## Another loader on the machine moves SlopOS's default

**Status**: Open
**Severity**: Low (shared firmware state)
**Component**: `userland/src/apps/bootctl.rs`, `boot-core/src/variables.rs:146-151`

`LoaderEntryDefault` is machine-wide: another loader that deletes it moves
SlopOS's default to Limine's first entry, and one that sets it to a name
Limine does not offer leaves `bootctl install` refusing until `bootctl
set-default`. A firmware entry
outside `BootOrder` past the first free number is not found, so registering
can add a second one.

---

## USB and keyboard rough edges

**Status**: Open
**Severity**: Low (each needs a device in a port)
**Component**: `usb-core/src/xhci/controller.rs:12-19`,
`drivers/src/usb/xhci/host.rs:282-312`, `drivers/src/keyboard.rs:154-260`,
`drivers/src/ps2/keyboard.rs:186-215`

- An unresponsive xHCI controller holds the `pci` boot step for up to 11 s
  (BIOS release and probe reset), each of up to eight.
- A device that toggles its connection logs an attach and a detach each time,
  unthrottled.
- Key events are not merged across keyboards: a pulled keyboard's Shift release
  is delivered while another keyboard still holds Shift, which consumers that
  read the modifier state ignore.
- On a machine with no xHCI controller, an LED byte the i8042 leaves
  unanswered is sent again only at its next byte or lock change.

---

## Serial and console input drops what follows a signal character in one burst

**Status**: Open
**Severity**: Low (typed input lost)
**Component**: `drivers/src/tty/io.rs:30-58`, `drivers/src/tty/ldisc.rs:1588-1612`

`receive_buf` stops at the first signal-generating character and reports how
many events it consumed; `drain_hw_input_locked` discards the rest of the
burst it drained from the hardware.

---

## Peer identities that outlive what they describe

**Status**: Open
**Severity**: Low (no current path; a hazard for a new trusting reader)
**Component**: `slopos-ostd/src/process/identity.rs`, `mm/src/process_vm.rs:2924-2946`,
`userland/src/apps/init/extensions.rs` (`bind`)

- A connector to an extension service's socket reads init's identity, recorded
  at `listen`, through `SO_PEERCRED` and `SO_PEERPROGRAM`. privd and the
  compositor trust init's identity only on descriptors at fixed numbers init
  hands them at start.
- A descriptor a uid-0 base connector passes on speaks as root while the
  connector runs its program.
- An `mprotect` that makes an executable page writable keeps the program, so a
  base program that writes its own code keeps its identity; only a page newly
  made executable forfeits it.

---

## Kernel interface edge cases

**Status**: Open
**Severity**: Low (no process gains over another)
**Component**: as listed

- `stat` reports `st_dev` 0 for every file (`fs/src/vfs/traits.rs:126`), so
  userland's `(st_dev, st_ino)` checks — privd's `still_at`,
  `sealed::as_written`, `tree::stage` — cannot tell two filesystems apart.
- `SpawnAttrs` carries no size (`abi/src/spawn.rs:57`): a program built
  against a shorter layout passes stack bytes as the newer fields, bounded by
  its mask, `SetId` and the `set` bits it names.
- A ring's `OP_CONNECT` checks the name's mode against the submitter as
  `ring_enter` found it and attests the identity as of each try
  (`ring/src/enter.rs:209-217`).
- `socketpair`, `accept` and `SO_PEERPIDFD` install a descriptor before
  writing its number out and close it by number if that fails, which a
  sibling racing `dup2` onto the number sees
  (`core/src/syscall/net_handlers.rs:230-239`, `:355`, `:395`, `:709`).
- A sibling thread survives `execve` and runs on in the new image
  (`core/src/exec/mod.rs:698-704`); the exec then attests nothing.
- A pidfd turns readable when its process's leader thread exits, while other
  threads may still run (`pidfd/src/file_ops.rs:19-23`).

---

## Typed secrets stay in buffers until reused

**Status**: Open
**Severity**: Low (readable by no other process)
**Component**: `fs/src/pipe.rs:207`, `userland/src/apps/compositor/input.rs:198-247`

A pipe's buffer is zeroed when allocated, not when freed, and the compositor's
raw input batch is not wiped, so a password typed into a question or relayed
to a mechanism stays in kernel pipe pages and the compositor's memory until
they are reused.
