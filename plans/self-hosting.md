# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

Nothing in the tree states this goal today. This document is the anchor; the
phases below are ordered by what physically blocks the next measurement, not by
appetite.

**Scale, from the host, measured:** the pinned toolchain sysroot is 1.1 GB;
`librustc_driver.so` is a single 161 MB shared object loaded through `PT_INTERP`;
`builddir/target` holds 52 GB across 219,895 files. Against that, SlopOS now
runs a 24 MiB executable with a gigabyte of anonymous address space and a
demand-paged file mapping, resolves a 4096-byte path with symlinks in it, stats
a file for a real mtime, mounts a 16 GiB volume holding a million inodes — this
repository and that sysroot among them — and refuses `PT_INTERP` outright
(`mm/src/elf.rs`). The remaining gap is one order of magnitude in linking, and
every constant that produced the storage gap was chosen correctly for an
appliance.

**The theme of this plan:** SlopOS's limits are not architectural mistakes, they
are appliance-sized constants and appliance-sized policies. A workbench needs
those quantities derived from the medium (image size, RAM, file size) instead of
frozen at values that fit a test fixture. The work is mostly *widening under
proof*, not redesign — with two remaining exceptions (dynamic linking and the
compiler bootstrap itself). Eight more, a page-fault path that can reach the
device, a POSIX floor a build system can stand on, a filesystem that can hold a
tree, a utility set that is executables rather than shell builtins, a shell
a build script can be written in, a terminal an editor can be written
against, an editor written against the machine itself, and a target that is a
unix-family Rust target over a real C library, have landed.

## Architectural constraints (do not violate)

- **Unsafe surface.** Only `slopos-ostd` may use `unsafe`; every other kernel
  crate stays `#![forbid(unsafe_code)]` and `check_unsafe_expansion.sh` sees
  through macros. Nothing in this plan earns an exemption.
- **Allocation discipline.** `KBox`/`KVec`/`KArc`/`KBTreeMap` only. Every
  toolchain-sized buffer this plan touches must become a chunked or page-list
  design rather than a bigger single allocation: `MAX_ALLOC_SIZE` is 1 MiB
  (`mm/src/slab/mod.rs:61`) and raising it is not the fix. The verity hash
  array, the attest bitmap, a ramfs file's body and the glyph atlas are all
  chunked for exactly this reason and are the pattern to copy.
- **Stack frames ≤ 2 KiB** against a 4 KiB guard page. This is why a 4096-byte
  path lives in a `KVec` — in `CanonPath`, in `UserPath`, and in the shell —
  rather than in an array on a frame, why `NameBuf` borrows from the canonical
  path instead of copying out of it, and why `BlockCache` and `Journal` are
  built with `KBox::try_init` instead of by value.
- **Task ownership I1–I8** and **no `async fn` in a kernel crate**. The
  sleepable fault path this rests on is a blocking task on its own kernel
  stack, not an executor.
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source, ever
  — which ruled out lifting busybox-lineage utilities or Linux userland code
  for the utilities, so the multicall *shape* was taken and every line
  written here. The same rule ruled busybox `ash` out of the shell: the
  grammar below is written from the POSIX Shell Command Language (IEEE Std
  1003.1 §2.3–2.14), which is a specification rather than an implementation,
  and the architectural references consulted for it — dash (BSD-3), toybox
  `sh` (0BSD), mrsh (MIT) — are compatible and were read for shape, not text.
  Concepts, ABI numbers and struct layouts are free to
  take; prose and implementation are not. Anything new linked into a shipped
  binary needs a `NOTICE.md` entry. Fonts stay runtime-loaded.
- **Ratchets are measurements, not numbers.** Every phase here grows the stack,
  quota, lockdep, test-count and filesystem-cost pools. Re-measure with each
  gate's `--emit-allowlist` in the same commit and say which change added the
  delta.
- **`just boot`'s `verity=require` keeps meaning what it says.** The shipped
  image stays v1-verified and read-only. Everything this plan makes writable is
  a different medium.

---

## The loop this plan starts from

`just boot-persist` is a machine whose state survives, including a rude QEMU
exit, and no host build destroys guest data. Every measurement below depends on
that, because a root that silently reverts to RAM makes each of them a
measurement of the initramfs.

What it rests on, in case a later phase disturbs it:

- **The image is marked clean while it is idle.** The flusher calls
  `mark_filesystem_clean` (`fs/src/ext2_vfs.rs`) on every pass that leaves
  nothing dirty, nothing unbarriered, no superblock drift and an empty log, and
  `Ext2Fs::transaction` re-stamps `EXT2_ERROR_FS` before the next mutation
  reaches the device. That is ext4's freeze/thaw ordering, arrived at
  automatically rather than through `fsfreeze`. A rude exit costs the last idle
  window instead of an image that mounts read-only forever, which `root=auto`
  would then demote to `/mnt` while booting the initramfs.
  `test_ext2_clean_stamp_thaws_before_the_next_write` holds the ordering to the
  offset of the first device write.
- **The host refuses rather than rebuilds.** `build_fs_image.sh` holds a
  `PRESERVE_FS_IMAGE=1` image to `check_fs_image.sh` — sound *and* clean, since
  `e2fsck -fn` alone exits 0 on a dirty superblock — and an image it cannot
  keep stops the build with the command that repairs it. `just
  boot-persist-reset` is the only path that deletes one. A larger
  `PERSIST_IMAGE_SIZE` grows the existing image through `resize2fs` with the
  trailer kept aside, so a failed resize leaves it byte-identical.
- **A rebuild no longer re-blesses guest writes.** `gen_verity.py` AND-s the
  old attested bitmap into the new one, so a block a boot rewrote stays
  un-attested across rebuilds — what `fs/src/verity.rs` already said the bitmap
  meant.
- **The persist root is 512M against a 32M shipped image, and 1 GiB is no
  longer the ceiling.** The verity hash array is chunked at 4 bytes per 4 KiB
  block in 256 KiB pieces, so what bounds an image is the resident hash a
  machine's RAM can hold — which the mount computes and *refuses* past
  (`VerityError::TooLarge`) rather than discovering as an allocation failure.

---

## A large program runs

The second thing this plan rests on, and the reason the phases below are
*measurable*: a toolchain-sized process can start. `bigprog_test` is the
standing proof — a 24 MiB binary that loads intact, maps a gigabyte of
anonymous space, maps a file past a ragged EOF, recurses through three
megabytes of stack and forks with 192 MiB resident. Every one of those was a
hard refusal before, and the last two are sized deliberately: 3 MiB is past the
whole of the old fixed stack, and 192 MiB is past the ~170 MiB at which `fork`'s
single-`KVec` snapshot used to exceed the slab's 1 MiB ceiling and panic. The
suite's guest is 1 GiB rather than 512 MiB to afford that residency.

What it rests on, in case a later phase disturbs it:

- **A user page fault can reach the device.** #PF has no IST entry
  (`boot/src/ist_stacks.rs`), so a user-mode fault lands on the faulting task's
  own kernel stack via `TSS.RSP0` — exactly as a timer IRQ from user mode
  already did — and `vector_uses_ist` excludes vector 14 so no preempt hold is
  taken. The handler re-enables interrupts, leaves interrupt-nesting context,
  resolves, and hands off to the scheduler at trap exit. That is what makes a
  blocking `fs.read` legal on the fault path, and it is a *blocking task*
  rather than an executor, as the constraints above require. The cost is
  stated: a kernel #PF taken with no room to push its frame now escalates to
  #DF, which `exception_double_fault` classifies against the IST, exception
  data-stack, emergency and task-stack guard pages.
- **The per-process lock is not held across the I/O.** A file-backed fault
  plans under `PROCESS_VMS[slot]` (`demand::plan_file_fault`), drops it, reads
  one page through `filemap::fault_page_in_set`, then re-takes the lock and
  re-validates that the region still names the same file page before installing
  (`demand::install_file_page`). A mapping unmapped or replaced mid-read
  resolves as a retry, not as a page from the wrong file.
- **`exec` stages a header, not an image.** `do_exec` reads
  `ELF_HEADER_WINDOW` (7232 bytes — the ELF header plus the largest
  program-header table), validates every segment extent against the file's real
  length, maps each `PT_LOAD` zeroed under the lock, then streams the file into
  the mapping in `EXEC_READ_CHUNK` pieces with the lock dropped. Kernel memory
  per `exec` is therefore independent of the binary's size; `EXEC_MAX_ELF_SIZE`
  is 512 MiB and, unlike the 16 MiB it replaced, reachable. The *mapping* is
  still eager, so what an image may ask the frame allocator for is bounded from
  the other side too: `MAX_TOTAL_ZERO_FILL_SIZE` caps the `p_memsz`-past-
  `p_filesz` part at the 256 MiB the whole-image ceiling used to permit, or a
  one-page ELF declaring a 2 GiB `PT_LOAD` would have an unprivileged `exec`
  memset half a million frames under the per-process lock. The relocation pass
  that forced whole-file staging is gone: it only ever ran for an image not
  linked at `PROCESS_CODE_START_VA`, nothing the tree builds is, and it relied
  on the loader handing it physically contiguous frames. Such an image is now
  refused (`ElfError::UnsupportedLoadBase`) rather than loaded unrelocated.
- **Mappings are lazy, and the ledger says which are resident.** `brk`, `mmap`
  and both file-mapping modes install no PTE; the fault installs one page. The
  stack is 1 MiB mapped inside an 8 MiB lazy extent with a guard gap below it,
  so a deeper stack is an ordinary demand fault and one past the ceiling finds
  no VMA. `Pages` is 4 GiB of VA per process and means what it always meant —
  `RLIMIT_AS`, not memory — and the new `ResidentPages` axis is the other
  number: the count of present user leaves, maintained by the page-table cursor
  because that is the only place one appears or disappears. The suite measures
  ~113 000 resident against ~950 000 mapped — a factor of eight, which is
  demand paging working rather than a discrepancy. Page tables are charged too,
  so a multi-gigabyte address space is no longer free.
- **`fork` fails rather than panicking.** The parent's PTE snapshot is chunked
  at `CLONE_CHUNK_PAGES` and every push is fallible, so a parent with a
  gigabyte resident gets `ENOMEM` where it used to take the kernel down on a
  `KVec` over the 1 MiB slab ceiling. `VmaMap::remove_range` and `drain` are
  allocation-free for the same reason: `munmap` and teardown have no failure
  channel. Fork's cost is still O(resident) in kernel memory, and deliberately
  so — the whole parent walk happens under one hold of its lock, because a
  parent whose other threads could write between the COW mark and the child's
  mapping would not be handing over a snapshot.
- **`mprotect` splits.** `VmaMap::split_at` / `protect_range` / `coalesce_at`
  carve the range out, rewrite only it, and re-merge the boundaries, so a
  sub-range no longer rewrites its whole VMA's recorded protection while
  touching only some of its PTEs. A file-backed VMA rebases its file offset on
  every split, or a fault in the tail would read the wrong page — and
  `can_merge_before` compares those offsets for contiguity, so restoring a
  protection re-merges the halves instead of leaving an entry per call.
- **`exec`'s argument surface is byte-bounded.** `EXEC_MAX_ARG_PAGES` is the
  same 128 KiB the retired 32-argument cap implied, now spendable as many short
  strings; `EXEC_MAX_ARG_STRINGS` is only a loop bound so a NULL-less user
  array terminates. `setup_user_stack` resolves the address space once instead
  of per write.
- **The filemap caps come from the medium.** 128 inodes, and a page ceiling of
  a quarter of usable frames with the old 1024 as the floor, because a page
  under a live user PTE is unreclaimable by construction. A page whose *start*
  is past EOF is refused per page; one straddling EOF is zero-filled, which is
  what makes a mapping of a file whose size is not page-aligned legal.

**Two clauses of this did not land, and are not hiding.** There is still no OOM
*disposition*: a fault that cannot find a frame kills the faulter with a
SIGBUS-coded exit, and choosing a victim instead is a policy subsystem rather
than a constant. It belongs with swap, which also did not land — see the
storage section below, which is where both were expected to arrive. And user
mappings are all 4 KiB; nothing instantiates the 2 MiB leaf the page tables
already support, which is a throughput item, not a capability one.

---

## A build system's floor is in place

The third thing this plan rests on: a program can find its files, learn whether
they changed, spawn children and know how they died. `buildctl_test` is the
standing proof — an in-guest build driver that creates a source tree under a
relative path with names past 32 bytes and a symlinked include directory,
spawns a stub compiler per file with `Command::current_dir` set, fingerprints
each input by `mtime`, skips every unchanged input on a second run, recompiles
exactly the one input it touched, reads a child's real exit code, sees a child's
`SIGSEGV` as a signal rather than as exit 139, and holds a `flock` a second
attempt cannot take. Every one of those was a wrong answer before.

What it rests on, in case a later phase disturbs it:

- **A path is 4096 bytes and can contain a symlink.** `MAX_PATH_LEN` and
  `USER_PATH_MAX` are `PATH_MAX`; `MAX_NAME_LEN` and `USER_NAME_MAX` are ext2's
  own 255, which is what a `libcore-<hash>.rlib` needs. The walk
  (`fs/src/vfs/path.rs`) stats each component, keeps a stack of the ancestors
  it has resolved, and on a symlink splices the target and restarts — so the
  budget is `MAX_SYMLINK_FOLLOWS` (40) for the *whole* resolution, as Linux
  has had it since 4.2, and exhausting it is `VfsError::TooManySymlinks` →
  `ELOOP`. `..` pops that ancestor stack rather than being folded away
  lexically before the walk, which is the only way it can mean what POSIX says
  it means once a component can be a symlink: `a/link/..` is the directory
  holding the link's *target*, not the directory holding the link.
  `RESOLVE_NOFOLLOW_FINAL` is what makes `lstat` and `AT_SYMLINK_NOFOLLOW`
  expressible, and `RESOLVE_MUST_BE_DIR` — set by a trailing slash or
  `O_DIRECTORY` — is what makes `open("file/")` the `ENOTDIR` POSIX requires.
- **No path is a stack frame.** A 4096-byte array on a 2 KiB frame steps clean
  over the 4 KiB guard page in one instruction, which no allowlist can raise.
  `CanonPath` is a `KVec<u8>`, and the walk builds the resolved canonical path
  incrementally into another rather than carrying a per-component offset table.
  `UserPath` (`core/src/syscall/args.rs`) stages a path syscall's argument on
  the heap, which is what Linux's `getname()` does for the same reason, and
  answers `ENAMETOOLONG` rather than truncating into the name of a different
  file. `NameBuf` borrows the final component out of the canonical path the walk
  already allocated, so `resolve_parent` costs one allocation rather than two
  and carries no inline 255-byte array. The shell's own 256-byte ceiling is
  gone the same way, or `cd` into a registry path would still fail on a machine
  that can now resolve one.
- **Relative paths resolve against the caller's cwd.** Every path syscall goes
  through `resolve_path_at`/`resolve_parent_at` with the cwd from
  `SyscallContext::with_cwd`, which is the only place a handler may read one,
  and `exec` and `spawn_path` resolve the program the same way — a `Command`
  with a relative program and a `current_dir` is the shape a build driver
  actually has. The task's cwd is heap-backed at 4096 bytes, and `chdir`
  resolves against the *old* cwd and requires a directory before storing the
  *walked* path — an unvalidated or lexical cwd is a correctness bug the moment
  the VFS consults it. A `dirfd` likewise stores the path its walk ended on,
  and every `*at` call re-checks that it still names the descriptor's own inode
  before resolving against it: a base renamed underneath answers `ESTALE`
  rather than silently redirecting into whatever holds that name now, which is
  the race the `*at` family exists to be immune to.
- **`stat` is the Linux `struct stat`.** 144 bytes, field-for-field, every hole
  a named field because `copy_to_user` copies `size_of::<Self>()` raw bytes.
  `FileStat::fill_user_stat` is the single producer and `FileType::to_s_ifmt`
  the single type mapping, which is what closed the bug where `fstat` on a
  regular file reported `FS_TYPE_DIRECTORY`: the enum's discriminants and the
  ABI's constants never agreed, and two call sites each had their own table.
- **The wall clock is real, and a timestamp can be set.** A CMOS RTC driver
  (`drivers/src/rtc.rs`, over a safe `slopos_ostd::io::CmosRegs` window whose
  serialisation obligation is a witness type rather than prose) is what the
  boot step prefers, with Limine's one-shot date as the fallback; neither
  answering still leaves `realtime_ns()` as `None`, which `fs/src/ext2/time.rs`
  depends on to decline to stamp rather than claim 1970. `clock_settime` is
  gated on `Capability::Clock` — not `Power`, which reaches a power primitive
  and must keep meaning that — and `set_realtime` refuses an out-of-range value
  instead of silently no-opping. ext2 and ramfs both implement
  `FileSystem::set_times`, so `utimensat` works on either root, and cargo's
  whole fingerprint model is mtime-based.
- **`waitpid` is POSIX.** `(pid, status, options)`, returning the reaped pid and
  writing `(code<<8)|sig`. slibc was already sending those three registers; the
  kernel read the status pointer as flags and never wrote it, so `ExitStatus`
  was `Some(0)` for every child and **every failed compiler reported success**.
  `WNOHANG` with a live child now returns 0 rather than `EAGAIN`, which no
  longer collides with a child that exited 0 — the bug that made `try_wait`
  never complete. `ExitInfo.signal` and `TaskExitReason::Signalled` are what let
  a signal death be told from `exit(139)`.
- **A stop is a state, not a dropped bit.** `TaskStatus::Stopped` exists;
  `task_group_stop`/`task_group_continue` park and resume every member of a
  thread group; a stopped task holds no runqueue position, is not reapable, and
  still parents its children. A member that is executing is poked and parks
  itself at its next return-to-user boundary rather than being descheduled
  mid-syscall from another CPU. `WUNTRACED`/`WCONTINUED` report it exactly once,
  which is what makes the shell's `fg`/`bg` able to resume a job and Ctrl-Z able
  to suspend one. `kill(pid)` fans out over the thread group, and `exit_group`
  exists because `exit` is the right primitive for a thread and the wrong one
  for a process.
- **A fault can be caught.** A user-mode fault posts `SIGSEGV`/`SIGBUS`/`SIGILL`
  with a `si_code` and a `si_addr` and returns to the trap exit, where the
  existing delivery hook builds the frame; the default disposition still kills,
  which is the same outcome as before rather than a regression. The frame is
  `[restorer][SignalFrame][UserSiginfo][UserUcontext][FPU]` — `SignalFrame`
  stays immediately above the restorer word, because both userland restorer
  trampolines document and depend on RSP pointing at it once the handler's `ret`
  pops the restorer. `sigaltstack(2)` is what makes a handler for a fault caused
  by stack exhaustion deliverable at all, and `MINSIGSTKSZ` is pinned to the
  real frame total by a const assert. A frame push that cannot be written
  terminates the task — immediately for a fault signal, whose interrupted
  instruction would re-execute and fault again, and on the second consecutive
  failure for any other, so an ordinary `SIGTERM` whose frame cannot be pushed
  is retried once and then fatal rather than re-pended at every boundary
  forever. slibc maps a `PROT_NONE` guard page below every thread stack, so
  std can tell a stack overflow from an ordinary `SIGSEGV`.
- **Threads share what POSIX says they share.** `CLONE_SIGHAND` was validated
  and then ignored, so every thread got a private action table while
  `pthread_create` asked for a shared one; the table now lives behind a
  `KArc<SigHandTable>`. The futex decodes `op & FUTEX_CMD_MASK`, so
  `FUTEX_PRIVATE_FLAG` — which every std- and glibc-shaped caller sets, and
  which used to make the whole call `ENOSYS` — is accepted; the timeout is a
  real `timespec`, relative for `FUTEX_WAIT` and absolute for
  `FUTEX_WAIT_BITSET`, and the bitset and requeue forms exist.
- **`std` reaches the syscalls that exist.** `read_dir` runs over `getdents64`
  on an owned directory descriptor rather than splitting the output of a
  bespoke listing call on newlines, so a filename containing a newline is just
  bytes; `symlink`, `read_link`, `hard_link`, `set_permissions`, `File::set_times`,
  `read_vectored`, `write_vectored` and `FileExt::read_at`/`write_at` are real
  instead of `unsupported`; and `Command::spawn` goes through `spawn_path` with
  a cwd in `SpawnAttrs`, so nothing allocates between fork and exec against the
  single global malloc spinlock a multithreaded parent could hand over locked.
- **The ABI is Linux's, numbers included.** A syscall number below
  `SYSCALL_PRIVATE_BASE` is Linux x86-64's number for the call of that name and
  carries that call's Linux signature: 114 of them, in a dense table sized to
  the 472 numbers Linux has allocated. The 37 operations Linux has no name for
  — the seat and screen acquisition, SlopRing, the compositor and cursor calls,
  the keymap and font uploads, the net-config surface that replaces netlink,
  `spawn_path`, the fate calls, the KTAP hooks, the fd-less console write and
  controlling-terminal read, and `sys_info` — live in a private range based at
  1024, which is the discipline ARM takes with `__ARM_NR_BASE` and is clear of
  both the allocated space and `__X32_SYSCALL_BIT`. Nothing borrows a Linux
  number for a shape it does not implement, and nothing keeps a private number
  once the shape agrees: `sendmsg`/`recvmsg` were private *because*
  `MsgHdr`/`CmsgHdr` were not Linux's layouts, and they took 46 and 47 the
  moment those layouts became Linux's — the allowlist that carried their
  justification is now empty. SlopRing still declines io_uring's 425-427,
  because its SQE is its own.
  `scripts/check_syscall_abi.sh` holds every non-private constant to Linux's
  own `syscall_64.tbl`, requires the private range contiguous, and fails a
  private name that collides with a Linux syscall unless the allowlist states
  why — so a number cannot drift from the call it claims.

**What this deliberately did not do.** The layouts are Linux's in all but the
places listed below, and those are what stand between source compatibility —
which this is — and *binary* compatibility with prebuilt Linux userland, which
is the open decision further down. Process-group waits (`pid == 0`,
`pid < -1`) are still `ESRCH`, because there is no process-group wait to answer
with. `st_uid`/`st_gid` exist for layout and read 0, which is the single-user
decision below, not an omission. `wait4` takes Linux's `rusage` argument and
refuses a non-null one, because there is no per-task resource accounting to
report.

Four divergences, stated rather than hidden:

- **The cwd is per-thread.** `CLONE_FS` is accepted and ignored, so a `chdir`
  is visible only to the thread that made it, where POSIX has the cwd per
  process. The buffer is a `TaskOwnCell` whose whole contract is that only its
  owning task reads or writes it, so sharing it means a lock, a lock class and
  a changed signature at every reader.
- **Five layouts are not Linux's**, and they are exactly the ones a binary
  compiled against Linux headers could not work around: `getdents64`'s `d_name`
  sits at offset 24 rather than 19 (the record header is naturally aligned
  rather than packed, which every in-tree consumer and its asserts agree on,
  and which `slibc`'s `opendir`/`readdir` now hide from a C caller entirely);
  `ucontext_t` is truncated (no `fpstate` pointer,
  `uc_sigmask` at 224 rather than 296) and `rt_sigreturn` restores from
  `SignalFrame` and ignores the ucontext, so a handler cannot redirect
  execution by editing `uc_mcontext`; `NSIG` is 32, so there are
  no realtime signals and therefore no glibc thread cancellation; `struct
  termios` is `termios2`-shaped behind `TCGETS`, which overruns a Linux-header
  `tcgetattr`; and `signalfd_siginfo` is 16 bytes rather than 128. Two more
  used to stand here and no longer do: `siginfo_t.si_addr` is at Linux's
  offset 16 and `msghdr`/`cmsghdr` are Linux's 56- and 16-byte forms, because
  std's unix PAL reads both and the target now rides it.
- **Advisory locks are 128 rows machine-wide**, shared by `flock(2)` and
  `fcntl(2)` record locks because they contend on the same file. A principal's
  share is bounded and a principal holding no lock can always take one, so no
  caller can deny locking to another; what the fixed table does bound is how
  many ranges one process may hold at once. Deadlock detection is the trivial
  self-conflict only: a two-process cycle parks both until a signal, where
  Linux answers `EDEADLK`.
- **Shared futexes are private-only.** The key is now
  `(address space, address)`, which is what stops one process reaching
  another's waiters; a genuinely *shared* futex needs the key to name the
  backing page rather than the mapping, which is a further change and not a
  flag decode.

---

## Storage holds a tree, and a write costs what it writes

The fourth thing this plan rests on: a volume two orders of magnitude past the
appliance root mounts, holds a working tree, and takes a write at a cost
proportional to the bytes it moves. `just test-capacity` is the standing proof
— a 16 GiB ext2 volume (4 194 304 blocks of 4 KiB across 128 groups, a million
inodes) carrying a checked-out copy of this repository *and* the pinned
toolchain sysroot, which the guest mounts, walks, searches and writes, and
which `e2fsck -fn` then accepts with a clean superblock. The numbers are on the
wire as `FSPERF[<phase>]` and `FSCAP[<phase>]`, and
`scripts/check_fs_throughput.sh` is the ratchet that keeps them there.

Measured on that volume: it holds **5 445 files and 1 105 385 677 bytes in
1 207 directories** — the host's staged tree to the byte — which the guest walks
in 1 634 device reads and reads back out of, not merely enumerates. The mount
costs **26–27 device reads** — geometry plus the journal's block map, not a
sweep — and a lookup of the last of 4 000 names in one directory costs
**0 block reads** once the directory index is warm. Measured on the appliance
root: 2 MiB written through the real `write(2)` path costs **10 transactions,
10 commit records, 44 block-layer write requests carrying 4 624 sectors, and 10
barriers**, identical across six runs of one ISO. Before this work the same
2 MiB was 512 transactions and 578 requests.

What it rests on, in case a later phase disturbs it:

- **Every appliance-sized constant is derived from the medium now.** The block
  cache's capacity comes from the volume's group count at mount
  (`cache_entries_for`, clamped to `[512, 8192]`), because a group's block and
  inode bitmaps plus the group-descriptor table are what every allocation
  re-reads — at 16 GiB that is 257 blocks against a floor of 512 slots, where
  the old fixed 512 would have spent half its capacity on them and evicted a
  bitmap it was about to need. The log is 1/64 of the image floored at 4M and
  capped at 64M (`scripts/build_fs_image.sh`), so the 16 GiB volume ships 16 383
  slots against the appliance root's 1 023. The verity hash array and attest
  bitmap are chunked at 256 KiB and streamed in against a running CRC, so no
  two full-size allocations are ever live and the trailer's bytes are unchanged.
  The ramfs derives its per-file ceiling and inode count from usable memory with
  the old 16 MiB / 4096 as *floors*, and its file bodies are page-chunked —
  without which the derived ceiling was a lie, since a single `KVec` body made
  the real limit `MAX_ALLOC_SIZE` and `/tmp` refused a 1 MiB object file.
- **A miss is O(1) and an insert is not O(n²).** The block cache evicts through
  an intrusive LRU rather than three linear scans of every entry, prefers a
  victim that is not a bitmap or a group descriptor, and drives its
  per-transaction bookkeeping from a preallocated list of the slots the
  operation touched instead of walking the whole cache. The journal answers
  "where does this block's newest copy live" from a chained hash index over its
  slot arrays — preallocated at attach, because **a commit must not allocate** —
  instead of scanning backwards from the head, which is what made a bigger log
  quadratic; `flush_revokes` and replay's revoke disposition go through the same
  index. Directory lookup and insert go through a bounded in-memory name index
  and a free-space hint: 1 600 names of 255 bytes into one directory used to
  visit 853 333 directory blocks and now costs **2 device reads**. The hint
  alone was not enough and the measurement said so — a growing directory still
  re-scanned its whole prefix every time a block filled — so each hint carries a
  *proof*, the size no block below it has slack for, and skipping the prefix
  requires the request to be at least that big. That is what makes the skip
  safe: it can never turn an insert that would have fitted into a new block.
- **The directory index is a cache, and its correctness rests on two rules.** A
  hit is a *candidate*: the record at that position is read and its name
  compared before it is trusted, so a stale entry resolves to nothing. The
  dangerous direction is the other one — a complete index missing a name — so
  `complete` is granted only by a walk that reached the directory's end with
  every record filed, every mutation must keep it, and anything that cannot
  drops the table. A failed transaction drops the index of every directory
  inode it touched, wired through the same op-touched bookkeeping the cache
  already keeps, because `Ext2Txn::drop` restores block contents the index would
  otherwise still be describing.
- **A write is one transaction per 256 KiB, and one request per run of
  blocks.** `IO_FILE_BATCH_SIZE` is the regular-file staging bound (`abi/src/io.rs`)
  and is deliberately not `IO_STAGING_SIZE`, which stays 4 KiB because for a tty,
  a pipe or a datagram that number is a latency decision. 256 KiB is 64 blocks
  per transaction, above the point where a transaction's data is written home
  and barriered once instead of going into the log, and well under the block
  cache so a batch cannot evict its own blocks. The user copy stays *outside*
  the mount lock, and must: `copy_bytes_from_user` takes the `PROCESS_VMS` slot
  lock, the file-fault path already runs PROCESS_VMS → drop → filemap →
  `CACHED_EXT2`, and copying under the mount lock would close that cycle. Then
  three write paths gather runs of consecutive blocks into one
  `BlockDevice::write_vectored`: the transaction's data write-home loop, the
  journal's record header plus its payload slots, and the log checkpoint's
  copy home. Ordered writeback survives because a run may only ever contain
  this operation's dirty data blocks, the commit record is still a separate
  write issued after every payload, and each phase still barriers once.
- **The check point is the lock owner's, not the operation's.**
  `Ext2Fs::transaction` used to notice the log was short of headroom and run an
  unbounded whole-filesystem `sync()` *inside* a hold of the mount lock, so
  every path walk on the machine queued behind it. The decision moved up to
  `Ext2Mount::with_fs`, which drives the chunked `sync_step` pass with the lock
  given back between steps and only then takes it for the operation; the
  in-transaction checkpoint survives as the last resort for callers that reach
  `transaction` without the VFS wrapper. The log's low-water mark scales with
  its capacity instead of being capped at 256 slots, or a batched transaction
  would fail with `NoSpace` on exactly the log size that was supposed to help
  it.
- **The block layer is four requests deep and 32 KiB wide.** `virtio_blk` built
  a fixed three-descriptor chain, bounced through two freshly allocated frames
  per request, and serialised every logical request machine-wide behind a
  `Mutex<()>`; a 1 MiB write was 256 round trips and 512 buddy allocations. Now
  a request is a `1 + N + 1` chain of up to eight data pages, each slot's pages
  are allocated once at probe so the steady state allocates nothing, and four
  slots are in flight at once — the arithmetic is stated and asserted, because
  `DEFAULT_QUEUE_SIZE` is 64 and shared with virtio-net and virtio-gpu:
  4 slots × 10 descriptors plus 2 quarantine chains is 60. Completion wakes one
  slot's waiter rather than broadcasting to all of them. Errors have variants —
  `Busy`, `Timeout`, `DeviceFault`, `Unsupported`, `OutOfMemory` beside the
  original four — with a bounded three-attempt retry on the retryable ones only,
  and a timed-out chain's pages move to a quarantine list while the slot gets a
  fresh page set, so a stall costs memory and a log line instead of permanently
  costing one of eight slots.
- **There is more than one filesystem.** The seven module statics that *were*
  the one ext2 instance are fields of an `Ext2Mount` value, and four of them
  live in a pool with **four separate `lock_class!` sites** — one class per
  instance, because `lock_class!` keys on its expansion site, a path walk
  crossing a mount holds one mount's lock while taking the next one's, and a
  shared class would make that legal nesting look like an unordered self-nest.
  Slot 0's class is still named `CACHED_EXT2`, so the class boot registers is
  the class it always was. `mount(2)` with `fstype=ext2` takes a `source`
  naming a block device, resolving it to devfs's read-only view for `MS_RDONLY`
  or to the device's exclusive write claim otherwise; `umount` of an instance's
  last mount flushes it, marks the image clean, drops the device and returns the
  slot, which is what gives the write claim back — a leaked claim answers
  `AlreadyClaimed` forever, and that is what the remount test exists to catch.
  One flusher serves the pool, taking one instance's lock at a time, and the
  reclaim hook stays `try_lock` per slot because waiting there would block on
  the I/O that needs the memory.
- **The cost is measured, and the measurement is a gate.**
  `slopos_fs::blockdev::stats` counts read and write requests, the sectors they
  carry, barriers, transactions and commit records at one relaxed atomic add
  each; `fs/src/fsreport.rs` puts them on the wire at the post-kernel-tests
  phase boundary, not from inside the measuring test, because a passing test's
  klog is not on the wire at the default verbosity — which is exactly the
  capture CI grades. The gate holds the per-MiB counts to caps, because they are
  deterministic for one ISO, and holds throughput to the quotient of the
  filesystem's write rate and the *same run's* raw block-device rate, because
  that is the only rate invariant under a change of accelerator; its self-test
  asserts that a uniformly three-times-slower machine still passes. A mount is
  graded in device reads per GiB rather than in seconds for the same reason.

**What this deliberately did not do.**

- **No swap, and still no OOM disposition.** Anonymous memory is never evicted;
  a fault that cannot find a frame kills the faulter. Both were expected here
  and neither landed: swap is a subsystem (a backing store, a PTE encoding, a
  reclaim policy and a victim choice), not a constant, and it belongs with
  whichever phase first needs a build to survive overcommit rather than being
  smuggled into a storage-sizing change.
- **No on-disk htree.** Directory scaling is the in-memory index above, so the
  on-disk format stays plain linear ext2 and `e2fsck` stays the oracle. The
  costs are stated: a lookup after a mount or a reclaim pays one scan to build
  the index, only four directories are indexed at a time, and a directory past
  the index's name cap falls back to scanning (with a test that proves it still
  works). The one thing that *had* to be handled is: an inode carrying
  `EXT2_INDEX_FL` hides its index inside records that look free, and
  `append_dir_entry` would have written an entry on top of an index node while
  `Inode::encode` faithfully kept the flag — so a directory a Linux host
  indexed is now **de-indexed** on its first mutation rather than corrupted.
  `Superblock::parse` also stopped ignoring `s_feature_compat` in silence: the
  bits the kernel understands are named, and a mount says which it is ignoring.
- **Mutations serialise per mount, not per inode.** Two mounts now proceed
  independently, and that is what `-j16` across `/` and `/home` buys; two
  writers to one filesystem still queue on that mount's lock. The waits are
  bounded rather than unbounded — that was the check-point work — but a
  per-inode design is a further change.
- **A write is still ~15% of the raw device's rate**, and the gap is request
  count and barriers rather than bytes: 4 624 sectors for 4 096 of data is
  1.13x amplification, while the reference write issues no barriers at all.
  22 block-layer requests per MiB is what the ratchet records.

---

## The utilities are executables

The fifth thing this plan rests on: a program that is not the shell can run a
utility. `coreutils_test` is the standing proof — a test binary that spawns
`/bin/<tool>` by path, reads back what it produced, and checks the status it
exited with. Before this, `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `diff`, `env`
and `ps` existed only as functions inside the shell
(`userland/src/apps/shell/builtins/`), so anything that spawned one got
`ENOENT`; `/bin` held GUI and network binaries and nothing a build could use.

**54 names in `/bin`, one 939 KB binary.** `/bin/coreutils` is a multicall
binary and each name is a symlink to it, so `argv[0]` selects the utility
(`userland/src/apps/coreutils/`, 13.5 kLoC). That is busybox's, toybox's and
uutils's shape, and the shape Asterinas ships in its own initramfs; Redox takes
uutils, which offers both a multicall binary and one binary per tool. The
alternative was measured rather than assumed: the smallest SlopOS binary in the
tree is 158 KB of std, slibc and unwinder before a line of its own code, so 54
of them would be ~8 MiB of a 32 MiB root for no behaviour at all. Each name
instead costs one inode and no block — `debugfs symlink` writes a *fast*
symlink, target inside `i_block` — and the initramfs carries the same set as
`newc` `S_IFLNK` records, which is why the utest passes unchanged under
`root=initramfs`. The installed set comes from the justfile's
`coreutils_tools`; the implemented set is the binary's own table; and
`coreutils --list` plus `coreutils_test` is what stops the two from drifting.

What it rests on, in case a later phase disturbs it:

- **There is one implementation of each utility, not two.** The shell's builtin
  table lost every file and text utility; what remains is what changes the
  shell itself (`cd`, `write`, `export`/`unset`/`set`/`env`, `jobs`/`fg`/`bg`,
  `kill`, `wait`, `exec`, `exit`, `time`, `help`, the control builtins POSIX
  requires of a shell — `:`, `.`/`source`, `eval`, `read`, `shift`, `return`,
  `break`, `continue`, `command`, `type` — and the SlopOS-specific
  `info`/`free`/`uptime`/`cpuinfo`/`random`/`roulette`/`wl`/`resolve`) plus the
  six POSIX resolves without a fork — `echo`, `printf`, `test`, `[`, `true`,
  `false` — which *delegate into the same functions* the `/bin` names run
  (`builtins/utility.rs`). Everything else the shell reaches through `PATH`
  like any other program, which is also what gives it correct job control: a
  Ctrl-C reaches a forked `yes` and could never reach an in-process one.
- **A utility writes to a `Sink`, never to fd 1.** That is the mechanism that
  makes one implementation serve both callers: the multicall binary points the
  sink at fd 1, and so does the shell, because the executor `dup2`s a `>`
  target onto fd 1 around a builtin and puts the shell's own back afterwards —
  so `echo hi > f` keeps working and is the same code path as `echo hi`. The
  sink also carries whether its destination is a terminal, which is why `ls`
  can lay out columns and colour on a tty and emit bare newline-separated names
  into a pipe — the old builtin printed `name (size)` and `(empty)`, which no
  pipeline could parse.
- **The semantics that were wrong are fixed, not merely present.** `cat` no
  longer stops at 512 bytes; `cp -r` and `rm -r` recurse (through one shared
  `Walk`, post-order for removal); `mkdir -p` creates parents; `diff -u`
  produces a unified patch and `patch` applies it — that round trip is a test
  case, not a claim. `grep` exits 0/1/2 and `test` 0/1/2, because a build
  driver reads a status rather than a message.
- **`execve` resets the thread pointer.** Making the utilities executables put
  a *fork-and-exec* on the path of every pipeline stage, and that path was
  broken: `execve` installed the new image's `FS_BASE` only when the image
  carried a `PT_TLS`, so an image without one kept the *old* image's thread
  pointer, and slibc's startup adopts a non-zero `FS_BASE` as an
  already-installed TCB (`slibc/src/thread/tls.rs`). `echo x | tee f` therefore
  faulted at `cr2=0x500000060` in `tls_init_main_thread` — a dangling pointer
  read, not a missing tool. The reset is now unconditional
  (`core/src/syscall/process_handlers.rs`), as Linux has it. Nothing before
  this change exercised the combination, which is why a builtins-only shell
  never saw it.
- **`canonicalize` resolves against the working directory.** The std port
  joined a relative path onto `/` (then in `slibc/std_pal/`, since deleted), so it
  answered the canonical path of a *different* file — and answered it
  successfully whenever that other file happened to exist. `cp`'s
  copy-into-itself refusal is what found it: the guard passed by accident for
  `.` and not at all for anything else. Fixed at the port, with the case in
  `cd_test`; `cp` still folds `..` lexically of its own accord, because a
  destination that does not exist yet cannot be canonicalised at all.
- **The set is the POSIX floor a build needs**: `ls cat cp mv rm mkdir rmdir ln
  touch stat install mktemp basename dirname which grep sed find xargs sort
  uniq tr cut head tail wc tee cmp diff patch printf echo test [ true false yes
  seq sleep env nproc uname whoami pwd date hexdump ps tar gzip gunzip zcat
  sha256sum stty less`. Three engines are written here rather than depended on,
  because the vendoring rule is nine third-party crates and a utility set is
  not a reason to change it: a POSIX regex engine (BRE and ERE, with a step
  budget, because the pattern is user input), RFC 1951 DEFLATE plus RFC 1952
  framing with the CRC verified on read, and FIPS 180-4 SHA-256.
- **The privilege story is unchanged.** `/bin` and `/sbin` stay sealed, so a
  name inside them cannot be replaced, and a symlink's content cannot be
  rewritten in place. The grant table is keyed on the path `exec` *resolves*
  to, which for all 54 names is `/bin/coreutils` — an entry with no grant. The
  visible cost is that a spawned utility's task name is `coreutils`, because
  `task_name_from_path` names the canonical path: `ps` shows the binary, not
  the name that was typed.

**Five divergences and costs, stated rather than hidden.**

- **A non-UTF-8 operand is refused, and the reason is now the utilities' own.**
  `OsStrExt` came with the unix family and `sys/os_str` already routed this
  target at the `bytes` implementation, so a byte-clean path is expressible
  here; what still refuses one is that every utility reads `std::env::args()`
  rather than `args_os()`. That is a change to 54 utilities, not to a target.
- **`ls -l` cannot report a mode, an owner or a link count.** `Metadata` on
  this target carries length, type, mtime and a read-only bit and nothing else,
  so the permission string is derived from the type and the link count prints
  as 1. `test -r/-w/-x` answer from existence for the same reason, which is
  also the single-user decision below: everything runs as uid 0 and the loader
  does not consult the mode bits.
- **`sed` has no branching** (`b`, `t`, `:label`), and `cp -p` preserves a
  file's mtime but not a directory's — there is no path-taking `utimensat` in
  the userland wrappers, only `File::set_times`.
- **`gzip` encodes with fixed Huffman blocks**, falling back to stored blocks
  when a block would not shrink, so its output is correct and reads everywhere
  but is larger than GNU gzip's. `inflate` handles all three block types, so
  what a host produced is readable here.
- **The shell links the utilities it does not run.** `shell.elf` went from
  640 KB to 1.37 MB, because `help` and completion walk the tool table and a
  table entry holds its `run` pointer, so referencing any of it keeps all of it
  alive. The alternative is a second, metadata-only table — the drift this
  design exists to prevent — so the ~730 KB is paid deliberately. A `/bin`-
  scanning completion would cost nothing and cover programs the table does not
  know, and is the right fix once Phase 1 makes new binaries a thing that
  happens in-guest.

---

## A script can loop, branch and substitute

The sixth thing this plan rests on: what a build system writes is a *script*,
and a shell that cannot loop, branch or substitute is not a workbench however
many utilities it can reach. `shell_script_test` is the standing proof — 33
cases against the real `/bin/shell`, 32 of them feeding it a script down a pipe
and asserting on the exact bytes it produces, and one driving it on a PTY
because the continuation prompt exists only on the interactive path. Between
them they run `if`/`elif`/`else`, `while`, `until`, `for` with and without
`in`, `case` with alternation, functions with their own positional parameters,
`break n`/`continue n`, `$(...)` and backticks, here-documents in all four
forms, `* ? [...]` globbing, the parameter-expansion operators, `$(( ))`
arithmetic, `"$@"` against `$*`, a twelve-stage pipeline, a hundred-word
command and a thousand-byte variable. Every one of those was a syntax error, a
wrong answer or a refusal before. Related properties share one shell
invocation deliberately: `MAX_PROCESSES` is 256, a run reaches ~170 before
this utest starts, and a spawn per assertion measured 243 processes held at
the phase boundary with the next dozen answering `ENOMEM`.

Everything pure about the grammar is `shell-core`, host-tested by `just
test-host`: token recognition, the syntax tree and the recursive-descent parser
over it, POSIX pattern matching, IFS field splitting, the `${...}` operator
split and the arithmetic evaluator — 77 tests that run in milliseconds with no
QEMU. What stayed in `userland/src/apps/shell/` is the part that has to talk to
the kernel: expansion's variable lookup and command substitution, pathname
expansion's directory walk, and execution.

What it rests on, in case a later phase disturbs it:

- **`Incomplete` is a third answer, and it is the whole mechanism.** The lexer
  and the parser distinguish *wrong* input from *unfinished* input, so one
  reader serves a multi-line script file and a PS2 continuation prompt alike:
  append a line, re-parse, run when the parse stops asking for more. The
  failure mode that shape has is precise, and a test found it — `for; do`
  answered `Incomplete` because "no word token here" and "no token at all"
  shared a branch, so the reader waited for the rest of a command that could
  never arrive and swallowed the remainder of the script with it. A malformed
  construct must be a syntax error.
- **Quoting is recorded per byte, not in band.** `QBuf` carries a flag vector
  beside the bytes: quoted (neither splits nor globs), and came-from-an-
  unquoted-expansion (splits). A sentinel byte answers the same question and is
  what several C shells use, but a sentinel collides with the arbitrary bytes a
  filename may hold. The flags are what make `IFS=:` split `$x` and not the
  literal `a:b` beside it, and `case '*' in "*")` compare an asterisk with an
  asterisk.
- **Field splitting only ever touches an expansion's output**, and `"$@"` puts
  a hard field boundary between parameters that survives it. The one case the
  bytes cannot answer is an empty result — `cmd $x` with `x` empty passes no
  argument and `cmd "$x"` passes one — so the splitter is told whether the word
  held a quoted byte at all. `"$@"` over an empty parameter list is the
  exception to that exception and contributes no field.
- **A command substitution's output is data.** `$(...)` forks, pipes, reads to
  EOF and strips every trailing newline; the bytes are then subject to
  splitting and globbing but never re-tokenized, so a `;` or a `>` among them
  is a byte the command receives. The inner text is lexed and parsed at
  expansion time, which is what makes `$( ... $( ... ) ... )` nest by
  construction rather than by a counter.
- **An unmatched pattern is left exactly as written, and a generated pathname
  has to exist.** There is no `nullglob`, a wildcard matches neither a leading
  `.` nor a `/`, and a field with no unquoted `*`, `?` or `[` is not globbed at
  all — so `rm *.o` in a directory with no object files runs `rm` with a
  literal argument rather than with none. A literal component *after* a
  wildcard is checked too, or `echo */nope` would hand the command one
  nonexistent path per directory instead of the word it was given. The matcher
  has a single backtrack point rather than a recursion per `*`: nine stars
  against forty bytes took eleven seconds the other way, and both a `case`
  subject and a `${x##pattern}` value are script-controlled. The walk goes
  through `std::fs::read_dir`, so a glob inherits the utilities' UTF-8-path
  divergence.
- **A here-document's writer is a separate process.** The body goes on a pipe,
  and one larger than the pipe's 4 KiB capacity would otherwise block the shell
  on its own read end before anything has read it. The writer is reaped *after*
  the descriptors are put back and never before, or a command that read only
  part of a long body would deadlock the reap.
- **Where a command runs is decided per command.** A builtin, a function and a
  compound command run in this shell, so `cd`, an assignment and a loop counter
  survive; an external program, a `( )` subshell and every stage of a
  multi-stage pipeline run in a fork. Redirections follow from that: applied
  around an in-shell command and undone afterwards, applied *in* the child
  otherwise, so a path that cannot be opened is the child's status and the
  shell's own descriptors are never at risk. The cost is stated: a subshell and
  a substitution are each a process, which is what `(cd x; make)` costs here
  and everywhere else.
- **A redirected builtin has one output mechanism, not two.** The executor
  `dup2`s the target onto fd 1 and restores the shell's own, which is what a
  compound command and an external child need anyway; the global "write here
  instead" descriptor the shell used to consult is gone, and with it the
  question of which of the two was in force.
- **Only exported variables reach a child.** A bare `FOO=bar` is a shell
  variable; `export FOO`, `export FOO=bar` and a `FOO=bar cmd` prefix are what
  put one in a child's environment. The table exported everything before, which
  is how a stray assignment changes what a configure script decides. It is also
  heap-backed and unbounded in both directions now: 64 entries of 256 bytes
  silently truncated a `CFLAGS` or a `PATH` with a dozen entries in it.
- **`set -e` does not fire inside a condition.** `if grep -q x f; then`,
  `a && b`, `a || b` and `! p` are tests, and a condition-depth counter is what
  keeps errexit from ending the script on one. `break`, `continue` and `return`
  reach the construct that can honour them through a requested control flow
  rather than a return value, because a builtin's signature is a status — which
  is also what makes `eval break` break the enclosing loop with no second
  mechanism.
- **The pre-expanded word list is still an entry point, and no longer
  double-expands.** `exec::execute_tokens` takes words that are already final —
  `time`, and the in-tree tests that drive a pipeline without writing one — so
  `time echo '$HOME'` passes the four characters it was given. A caller that
  wants `|` or `2>` says so with `push_operator`; deciding by *lexing* the
  bytes instead made `command echo '>'` a redirection with no operand.
- **Two things must see past a name's first meaning.** `command NAME` resolves
  blind to the function table, or the canonical wrapper
  `ls() { command ls -F "$@"; }` calls itself until the stack runs out; and
  `unset NAME` names a *variable*, touching a function of that name only when
  no such variable exists.
- **A redirection's backup is taken before its target is opened.** The kernel
  hands out the lowest free descriptor, so `3>out` in a shell holding only
  0/1/2 opens exactly fd 3 — a backup taken afterwards captures the file
  itself, the `dup2` is a no-op and the close that follows drops the only copy,
  leaving the command with fd 3 shut and the shell with the file leaked onto
  it. The open landing on the descriptor being redirected is then a no-op
  success rather than a copy-and-close.

**What this deliberately did not do.**

- **No `trap`.** A build script's `trap ... EXIT` cleanup is a real want and it
  needs a disposition table the shell consults at every exit path, not a
  constant. It belongs with whichever phase first needs a failed build to clean
  up after itself.
- **No `local`, no `getopts`, no aliases.** None is POSIX-required of a shell
  (`local` is not in the standard at all), and each is a scoping or parsing
  mechanism rather than a widening. A function's variables are the shell's.
  `set -o` exists, and names the same four options the letters do — `errexit`,
  `nounset`, `xtrace`, `noglob` — and nothing else.
- **None of the non-POSIX conveniences**: no `$'...'`, no brace expansion
  `{a,b}`, no `[[ ]]`, no arrays, no `select`, no `case` `;&` fallthrough.
  `>|` parses and behaves as `>`, because `set -C` does not exist to
  distinguish them.
- **Arithmetic is signed 64-bit and wraps.** An overflowing `$(( ))` gives what
  C gives rather than ending the script.
- **`time` is a builtin over pre-expanded words**, not the reserved word POSIX
  makes it, so `time a | b` times `a` rather than the pipeline.
- **`set NAME=VALUE`** is kept beside POSIX `set --`, because the shell
  accepted it before this and something in the tree may use it.

---

## The terminal is one an editor can be written against

The seventh thing this plan rests on: a full-screen program can read the
keyboard, read the mouse, ask the terminal what it is, and draw a frame around
what it shows. `terminal_grid_test` is the standing proof — an in-guest test
that drives the real encoder and the real emulator and checks the exact bytes:
F1 as `SS3 P`, F12 as `CSI 24~`, Ctrl+Left as `CSI 1;5D`, Up as `SS3 A` once
DECCKM is on, Shift+Tab as `CSI Z`, PageUp as `CSI 5~`, a left click as
`CSI <0;10;5M` and its release as the same with `m`, a bare move refused under
button-event tracking and reported under any-event, `CSI c` answered
`CSI ?1;2c` and `CSI >c` answered `CSI >0;1;0c` with nothing printed, and
`CSI 6n` answered with the cursor's 1-based position. Every one of those was a
dropped key, a wrong answer, a stray glyph or a refusal before.

What it rests on, in case a later phase disturbs it:

- **A key is identified by its canonical keycode, not by a pseudo-byte.** The
  keyboard driver bakes a legacy `ascii` code for nine navigation keys
  (`named_to_legacy_ascii`, `drivers/src/ps2/keyboard.rs`) and 0 for everything
  else, so F1–F12, Insert and Menu left the kernel already anonymous — and
  `classify` then discarded the canonical HID `keycode` and the per-event
  modifier byte the compositor had faithfully carried the whole way. Both were
  drops, not absences: `encode_key` now takes a `KeyPress`
  (`terminal-core/src/input.rs`) holding ascii, keycode, codepoint and mods. A
  baked navigation byte still resolves first, because it is the one thing that
  survives a keypad key whose layout meaning is navigation; everything the
  driver left anonymous — every F-key, Insert, KP-0-as-Insert — resolves from
  the canonical keycode, which is also a second source for the navigation block
  so a nav key no longer *depends* on a pseudo-code the rest of the system has
  to agree on. The unreachable scancode table that used to sit at the end of
  the encoder is gone: `legacy_scancode = byte & 0x7F`, so no arm above 0x7F
  could ever have matched.
- **The encoding is xterm's PC-style one, and the modifier is a parameter.**
  `1 + shift + 2*alt + 4*ctrl` in the second CSI parameter, so Ctrl+Left is
  `CSI 1;5D` and Shift+F5 is `CSI 15;2~`; F1–F4 are `SS3 P`–`SS3 S` unmodified
  and `CSI 1;mod P`–`S` modified; the editing keypad is `CSI n ~` with the
  modifier as its second parameter. DECCKM is honoured — the parser had tracked
  `cursor_key_mode` since it was written and nothing had ever read it — and only
  for an *unmodified* cursor key, because a modified one needs the parameter
  slot that `SS3` does not have. AltGr is excluded from the Alt bit: the kernel
  reports it with `MODIFIER_ALT` set as well, and counting it would turn the
  `@` an AltGr level resolved into a modified keypress.
- **Alt is a prefix, Shift+Tab is a sequence.** Alt+x is `ESC x` and Alt+ä is
  `ESC` plus the UTF-8, which is what every terminal does and what a line
  editor's meta bindings are written against. Shift+Tab is `CSI Z`, and the
  modifier snapshot is the only thing that can produce it: the keymap folds Tab
  and Shift+Tab to the same 0x09.
- **PgUp/PgDn belong to the application.** They were consumed locally for
  scrollback, so a full-screen program could not page. The local scrollback
  chord is now Ctrl+Shift+PgUp/PgDn, beside the Ctrl+Shift+C/V clipboard chords
  that were already terminal commands — and the kernel's own Shift+PgUp
  interception for the vconsole had to learn to require Shift *without* Ctrl, or
  the chord would never have reached a client at all.
- **Mouse reporting is the application's, and Shift is the way out.** DECSET
  1000/1002/1003 select press-only, drag and any-motion; 1006 selects the SGR
  encoding. The three tracking modes are one selector, as xterm has them, so
  resetting any of them stops reporting. While reporting is on, a pointer event
  drives the PTY instead of the local selection — unless Shift is held, which is
  xterm's override and the only reason a selection stays possible under a
  full-screen program. Motion emits one report per *cell crossed*, not per
  pixel, and the X10 encoding refuses a coordinate past 223 rather than
  truncating it into the wrong cell: it has one byte per field, and 1006 is the
  encoding with no such limit.
- **A query is answered on the turn it was asked.** `VtAction` gained
  `DeviceAttributes` and `DeviceStatus`, the grid gained a bounded reply queue,
  and the event loop drains it into the existing `MasterWriteQueue` immediately
  after `drain_master` rather than at the next wake — a program blocked reading
  a CPR would otherwise wait for a keystroke or a blink. The queue is 256 bytes
  and drops a whole answer rather than truncating one, because a half-written
  `CSI ?1;2c` is worse than silence. A reply is also not a cursor movement, so
  it must not cancel a deferred autowrap the way every other non-printing action
  does.
- **A CSI private marker is tracked rather than aborted on.** `?` was the only
  marker the parser knew; `>` dropped it back to Ground, so `CSI > c` printed a
  literal `c` into the grid the moment an editor probed for a secondary DA. The
  marker is now a byte, and dispatch is split by it — which also closed the
  quieter half of the same bug: the old dispatch consulted the marker for
  `h`/`l` and for nothing else, so `CSI ? 5 m` reached the SGR handler and
  turned a mode query into a blink attribute.
- **The shell's own decoder understands what the terminal now sends.** It
  matched `CSI A`/`CSI 3~` and a handful of literal forms, and answered
  `Partial` for anything parameterised until the buffer passed eight bytes — so
  one Ctrl+Left swallowed the next characters typed. It recognizes the whole
  CSI/SS3 shape now (parameters, intermediates, one final byte), and a
  well-formed sequence it has no use for is consumed *whole*: that is what keeps
  an F-key's tail from arriving as text. Ctrl or Alt on a horizontal arrow is
  word motion, sharing the boundary rule `CTRL_W` already deleted to. `ESC`
  plus a byte that cannot begin a sequence is the one form it re-emits instead
  of consuming: nothing here binds a meta chord, and dropping the pair would
  make Alt+x type nothing where it used to type `x` — and would swallow the
  lead byte of Alt+ä outright.
- **The glyph set is the blocks a TUI draws with.** 194 slots became 1190 over
  twelve ranges (`GLYPH_RANGES`, `font/src/lib.rs`): ASCII, Latin-1, Latin
  Extended-A, the spacing accents a dead key can flush, Greek, Cyrillic, General
  Punctuation, Currency, Arrows, Box Drawing, Block Elements and Geometric
  Shapes — which is what the shipped JetBrains Mono actually covers, so "non-
  Latin renders" means the scripts the font has rather than a promise it cannot
  keep.
- **Box drawing and block elements are drawn, not rasterized.** The atlas cell
  is derived from ASCII metrics and a glyph is centred on its advance and
  clipped, and JetBrains Mono's box glyphs do not span the em box — rasterizing
  them leaves a seam at every cell boundary, which is a framed TUI that looks
  broken. `font/src/boxdraw.rs` draws U+2500..U+259F procedurally instead, as
  kitty and wezterm do: one `Geom` derives the midlines and the light/heavy/
  double thicknesses from the cell once, every stroke goes through it, so a
  weight lands on identical rows in every glyph that carries it. The line block
  is a 128-entry weight table (four legs × none/light/heavy/double) plus one
  renderer rather than 128 hand-written cases; the eighths are
  `round(n * extent / 8)` so `█` equals `▀ | ▄` byte for byte; the shades are a
  4×4 Bayer dither, not a flat grey, so a shaded region reads as texture at any
  cell size. The boot console gets the same coverage, which the VGA 8×16 ROM
  font has none of.
- **The atlas is chunked, and a missing glyph is the notdef.** One
  `KVec::zeroed(GLYPH_COUNT * stride)` at 1190 slots and the ABI's largest
  32×32 cell is 1.2 MB, past `MAX_ALLOC_SIZE`; storage is now `KVec<KVec<u8>>`
  in 256 KiB pieces behind an `AtlasBuilder`, and the `SYS_FONT_SET` handler
  copies the user buffer into each chunk in turn instead of materialising the
  upload. `get_coverage` is two divisions and a slice index, still the per-cell
  hot path. A set codepoint the loaded font lacks now reads back the replacement
  diamond rather than a blank cell — without which growing the set by a thousand
  slots would have turned a visible notdef into an invisible one. The keying is
  `glyph_index(cp)` answering a *non-zero* glyph id: it answers `Some(0)`, never
  `None`, for a codepoint its cmap does not cover.
- **The measured cost.** At JetBrains Mono 16 px the cell is 10×22, so the
  atlas is 261 800 bytes in one chunk; at the ABI's 32×32 maximum it is
  1 218 560 bytes in five. The upload ceiling is `(GLYPH_COUNT + 1) * 32 * 32`
  and bounds *user* memory only. `net-core`'s hand-copied `is_renderable`
  mirror moved in lockstep, and its test asserts both ends of all twelve ranges
  rather than a sample.

**What this deliberately did not do.**

- **The kernel vconsole answers no query.** Its reply would have to reach the
  line discipline of the very TTY whose write lock it runs under, so a DA or DSR
  on `/dev/tty0` is ignored rather than answered wrongly; a program that queries
  there sees a timeout. Routing a reply through the deferred `PostLockWork` the
  echo flush already uses is the shape of the fix, and it is a TTY-layer change
  rather than a terminal one.
- **No `modifyOtherKeys`, no CSI-u, no Kitty keyboard protocol.** Ctrl folding
  happens in the kernel keymap, so `Ctrl+A` arrives as 0x01 and the terminal
  cannot report `Ctrl+;` at all — the kernel's `ctrl_transform` covers letters
  only. That is a keymap gap with a terminal-visible symptom, and the protocols
  that would expose it need the unfolded key, not a different encoder.
- **No focus reporting (1004), no 1005/1015 mouse encodings, no SGR-pixel
  (1016).** Focus needs a keyboard-focus event the compositor does not send a
  client; the other two are encodings nothing modern asks for once 1006 exists.
- **Bold is still a brighter colour and underline is still invisible.** A cell
  holds `{codepoint, fg, bg}` and the attributes are flattened into the colours
  at print time, so `SGR 4` is parsed, tracked on the cursor, and then dropped.
  Fixing it means an attribute byte per cell — `Cell` is 12 bytes across a
  100×240 grid plus two 1000-row rings — and `Cell::is_blank`'s definition,
  which the whole reflow trim rests on. Stated rather than hidden: an editor
  drawing with colour is served, one drawing with underline is not.
- **No astral-plane glyphs and no CJK.** The TTF parser reads cmap format 4
  only, so anything past U+FFFF resolves to nothing, and the shipped mono font
  has no CJK to cover even if it did.

---

## An editor you can work in

The eighth thing this plan rests on, and the last of the workbench: a file can
be opened, changed and written back without a Linux host in the path.
`/bin/editor` — Sloped — is a native GUI application: a file tree beside a
column of tabs, a code surface with syntax highlighting, find and replace, a
command palette, a file finder and an undo history, over `appkit` and the
compositor. `editor_test` is the standing proof — an in-guest run of every
`editor-core` case against the target's allocator, plus the application's own
state machine driven through the messages its widgets emit: open a file, type,
Save As, find and walk the matches, filter the palette and run what it lands on,
close a modified tab and be asked first, expand a directory in the tree, and
refuse a binary file.

It was written rather than ported, for the reason the plan gave: nothing
upstream is reachable before a C frontend exists, and an editor is where a
desktop OS earns its character. What that bought, beyond the editor, is a
toolkit that can express one.

What it rests on, in case a later phase disturbs it:

- **The logic is a crate, and the crate is host-testable.** `editor-core` holds
  the buffer, the cursor and its motions, the edit operations with their undo
  history, literal search, the fuzzy matcher and the syntax lexer, and it
  touches no syscall — the same split `terminal-core` and `shell-core` already
  draw, and the reason 91 cases run under `just test-host` in milliseconds and
  again in the guest under `just test`. The application above it owns the
  filesystem, the clipboard, the keymap and the window.
- **A line vector, not a rope, and the trade is stated.** What this edits is
  source code on a machine whose root filesystem is tens of megabytes: the cost
  that matters is per-keystroke work inside one line, which is O(line), and a
  line insert, which is a `Vec` move of `line_count` pointers. `MAX_LINES` is a
  million and a file past it is refused at load; `EDITOR_MAX_FILE_BYTES` is
  8 MiB. A rope buys a logarithm back at a complexity the whole crate would have
  to be tested against.
- **Positions are characters, never bytes.** The cursor, the selection, the
  renderer's column arithmetic and the search all count cells; one conversion
  (`TextBuffer::byte_of`) exists for the places `String` needs an offset. Tabs
  are the one place a *display* column diverges from a character column, which
  is why the code surface carries both and why a click inside a tab resolves to
  the side of it the pointer is nearer.
- **Undo is a transaction, not a keystroke.** Typing coalesces while it stays a
  run of single characters advancing from the last one; a motion, a paste, a
  save or a compound edit seals the group. Replacing a selection, splitting a
  brace pair, moving a line, commenting a block and the electric dedent a
  closing brace triggers are each *one* Ctrl+Z, because each opens a transaction
  around the several primitive changes it makes.
- **Highlighting is a line at a time, against the state the line above left.**
  A block comment or an unterminated raw string *is* that state, so a viewport
  costs the lines above it once and the viewport each frame — never the file.
  The cache lives behind a `RefCell` because drawing is a read that happens to
  memoize, and a view that had to borrow the document mutably would not be a
  view. Rust, C, TOML, JSON, Markdown, shell and Python are lexed; the lexer
  resolves what a *character run* is and never what a name means, which is why a
  keyword list and a delimiter table are enough.
- **The toolkit grew what the editor needed, and every application got it.**
  `appkit` had one font, fixed-cell, which is why every SlopOS application used
  to read as a terminal wearing a window. It now has two roles: proportional UI
  text (Inter, through `FontRenderer`) for every label, button, menu and header,
  and the fixed-cell atlas for content whose columns must line up. The new
  widgets are a virtualized code surface, a virtualized tree, editor tabs with a
  modified marker, an in-window menu bar, a focus-explicit line input, a
  draggable splitter, a card with a real shadow and a procedurally drawn icon
  set. The palette is One Dark.
- **Transient interaction state belongs to the application.** The widget tree is
  rebuilt on every message, so anything a widget remembered between a press and
  the move that follows it was already gone: a drag, a click run, a resize.
  Those now live in the application and are *given* to the widget, which is the
  same discipline the rest of the toolkit follows — and the reason a drag
  selection, a double click and a sidebar resize work at all.
- **A widget answers a key only when the key is its own.** Keyboard events are
  offered to every widget in turn until one consumes, and nothing ever sent
  `FocusGained`/`FocusLost` — so every widget's `focused` flag was permanently
  false, and the two that answered keys without consulting it answered *every*
  key. A button took Enter and Space from whatever was being typed into, which
  made a space unsearchable and a newline untypable while the find bar was
  open; a list took the arrows and emitted its row-chosen message, which for
  the command palette meant running each command the selection passed over.
  `run_app` now tells the widget losing focus and the one gaining it, and both
  widgets ask before answering.
- **A drag that leaves the thing it started on is still a drag.** State the
  application holds has to be *ended*, and a release is only an ending if it
  arrives. Two layers had to say so. A `StackWidget` now tells a move *and* a
  release to the children whose rect they missed, because a widget that latched
  on a press is the one waiting for them and every other widget guards on
  containment or on its own latch — a six-pixel splitter that hears about the
  pointer only while the pointer is still on it cannot be moved at all. Under
  that, the compositor holds the `wl_pointer` implicit grab: a
  press on a client's content pins pointer delivery to that client until every
  button is up, so dragging past the window edge no longer hands the pointer —
  and the release — to whatever is underneath. Without either, a selection goes
  on following a pointer with no button held, and the next keystroke replaces
  text the user never selected.
- **A space is a character.** The keymap reports Space as a *named* key so a
  focused button can be pressed with it, which meant no `appkit` text input
  could type one. Text-entering widgets translate it; buttons still get their
  chord.
- **The clipboard is the compositor's.** `windowing` gained the fd-based
  transfer both ways — a copy hands over a memfd, a paste is ask, be told the
  size, hand back a destination of exactly that size — and `appkit` exposes it
  as `clipboard::copy` and a `request_paste` whose answer arrives at
  `App::on_paste`. Control bytes other than tab and newline are dropped on the
  way in: a clipboard is untrusted input.
- **What the file had, the file keeps.** Line endings are detected and restored,
  a missing final newline stays missing, and indentation is read from the file
  rather than assumed — a tab-indented file indents with tabs, and a file
  indented two spaces stays that way. What is histogrammed is the *step into* a
  block, not the absolute indent a line carries, because every indent a file
  shows is a multiple of its unit and a four-space file with enough nesting
  shows as many eights as fours. A binary file is refused rather than opened as
  replacement characters that saving would then write back.
- **A save cannot destroy what it fails to replace.** `File::create` truncates
  before the first new byte lands, so a write that fails part way — a full
  image, a device error — would leave the user's file gone while the editor
  held the only copy. A save writes a sibling, `fsync`s it and renames over the
  target, which the journal makes one atomic metadata operation: what is on the
  medium is the old file or the whole new one, never a prefix of either. It
  also asks before writing at all when the file it is about to land on is not
  the one the tab read — changed underneath it, gone, or a Save As target that
  already exists — compared by modification time and length, recorded at open
  and re-taken at every save.

**What this deliberately did not do.**

- **No syntax tree, and no `syntect`.** The lexer is a hand-written one over a
  language table, because TextMate grammars want a regex engine and tree-sitter
  wants a C toolchain that does not exist yet. What that costs is precision a
  parser would have: a capitalized identifier is typed as a type, and a lowercase
  one before `(` as a call, because that is what a lexer can know.
- **No LSP, no multi-cursor, no split panes, no file watching.** Each is a
  separate feature with its own state; none of them is what "you can edit a
  file" needs. A file changed underneath the editor is therefore not noticed
  *while it is open* — the buffer does not reload and nothing marks it stale —
  only at the moment a save would overwrite it, which is where the damage
  would be.
- **The terminal editor was not written.** The plan offered either; the GUI one
  is what landed, because the toolkit gap it closed (a real text widget, a
  proportional font, a tree) is what every other SlopOS application needed too,
  and a TUI editor would have closed none of it. The terminal's own gaps named
  above — a per-cell underline, the unfolded Ctrl chords — are therefore still
  open, and still exactly what a full-screen program would find missing.
- **No shift-click without a keyboard focus.** The compositor sends no modifier
  state with a pointer event, as Wayland does not; `appkit` stamps a press with
  the keyboard's most recent snapshot, which is right while the window has focus
  and stale if the modifier was pressed before it got any.

---

## The target is a host's target

The ninth thing this plan rests on, and the one that makes the rest of Phase 1
a *toolchain* problem rather than a platform problem: `x86_64-unknown-slopos`
is a **unix-family Rust target over a real C library**, and `std` is upstream
`std`. What a SlopOS binary runs on is `library/std/src/sys/pal/unix`; the
20-file, 3 963-line bespoke platform layer that used to sit beside it is
deleted, and so is the 729-line script that used to sed it into the rustup
sysroot in place. `libc_abi_test` is the standing proof — fourteen in-guest
cases holding slibc to the libc module's own declarations: a zeroed
`pthread_mutex_t`/`cond_t`/`rwlock_t` usable without `_init` and a requested
stack size that reaches the thread, `read_dir` enumerating exactly what
`getdents64` does, a `sigaction` round trip through the 152-byte struct, a
`SIGSEGV` handler reading back the address it faulted at, an fd crossing an
AF_UNIX socket beside two non-adjacent iovec segments, `sysconf` agreeing
with the kernel on the page size and the CPU count, `environ` and `getenv`
being one environment, `realpath` resolving a relative link against the
link's own directory, and the sigset narrowing that turns the 128-byte
userspace mask into the kernel's 8-byte one. It exists because under this
design a wrong struct is a *miscompile* rather than a compile error, which
was the one new risk the decision carried — and two of its cases were
falsified before being believed, by breaking `si_addr` and the `SCM_RIGHTS`
count in the kernel and watching exactly those two fail.

What it rests on, in case a later phase disturbs it:

- **`restricted_std` was a string allowlist, and it is retired.**
  `library/std/build.rs` compares `CARGO_CFG_TARGET_OS` against ~45 names and
  emits `--cfg restricted_std` on a miss, which flipped the whole crate from
  `#[stable]` to `#[unstable(feature = "restricted_std")]` and forced that
  attribute into **65** files. One line — `|| target_os == "slopos"` — retired
  all 65 of them.
- **The sysroot is owned, not mutated.** `-Zbuild-std` resolves std from
  `<host sysroot>/lib/rustlib/src/rust/library`, so the old flow had no choice
  but to edit the rustup toolchain in place: a shared, unversioned, silently
  drifting input. `scripts/make_slopos_sysroot.sh` instead `cp -al`s the pinned
  toolchain (0.5 s cold, 0.04 s warm, one inode per file), replaces
  `lib/rustlib/src` with a real copy, applies the two fork patches, and
  registers the result with `rustup toolchain link slopos`. Builds are
  `cargo +slopos`.
- **The forks are diffs, and the diffs are the PRs.**
  `toolchain/rust/0001-slopos-std.patch` is 665 lines over 14 files of
  `library/`; `toolchain/libc/0001-slopos-libc.patch` is 1 550 lines over 6
  files of a pinned `libc 0.2.189`. Nothing is vendored: the libc tarball comes
  from the local cargo cache or from static.crates.io against the checksum in
  `toolchain/PIN`, and each patch is verified against its pinned SHA before it
  is allowed to touch a file. That is what makes "upstream it and the fork
  shrinks to nothing" an end state rather than a hope.
- **`libc/src/unix/slopos/mod.rs` is one file, and every layout in it is
  Linux's.** Redox's equivalent is 1 450 lines; this is 1 461 — the type
  aliases, the structs, the constant blocks, the `CMSG_*`/`FD_*`/`W*` helpers,
  `impl siginfo_t`, and one `extern "C"` block declaring **227** entry points.
  SlopOS's ABI already *was* Linux's at the numbers and the constants, so the
  module is not a translation layer; it is a statement of what slibc owes.
- **The std diff is cfg-sites, not a parallel PAL.** 665 lines over 14 files,
  of which three are new: `os/slopos/{mod,raw,fs}.rs`, the `MetadataExt`
  surface every unix target owns, derived from upstream's own `os/redox/`
  ones. A fourth, `sys/random/slopos.rs`, is a dozen lines over `getrandom`,
  exactly redox's shape. Everything else adds a `target_os = "slopos"` arm to
  a list that was already there: the `restricted_std` allowlist, `os/mod.rs`
  and `os/unix/mod.rs`'s platform module, `sys/args/unix.rs`'s stored-argv
  set, `sys/random/mod.rs`, `sys/thread/unix.rs`'s `set_name` and
  `available_parallelism`, and `sys/io/error/unix.rs`'s `__errno_location`
  spelling. Two arms *subtract*: `current_exe` is `Unsupported`, because
  SlopOS has no procfs and a process cannot name its own image; and
  `backtrace`'s unix arm is declined, because a userland binary is
  `panic = abort` with `.eh_frame` discarded by its linker script, so the
  libunwind `_Unwind_*` the unix arm references is not there to link against.
- **Synchronisation rides pthreads, which costs the std fork nothing.**
  `sys/sync/mutex/mod.rs`'s `target_family = "unix"` arm already selects
  `pthread`, so `Mutex`, `Condvar`, `RwLock`, `Once` and thread parking are
  slibc's futex-based primitives with no cfg-site at all. The price is paid on
  the slibc side, which is the right side: those objects grew to their declared
  glibc sizes (`pthread_mutex_t` 16 → 40, `pthread_cond_t` 16 → 48,
  `pthread_rwlock_t` 8 → 56, `pthread_attr_t` 16 → 56) and must work
  zero-initialised, because std allocates them with `PTHREAD_*_INITIALIZER`
  semantics and only sometimes calls `_init`.
- **slibc is a C library now, not a Rust crate that happens to export C
  symbols.** `libc.a` comes from a `slibc/staticlib` wrapper package whose
  whole content is the dependency plus a `#[panic_handler]`, and `crt0.o` from
  a `slibc/crt0` crate carrying the `_start` that used to live in
  `userland/src/lib.rs` — which is the single gate that turns "can a C program
  be built here" from *no* into *yes*, pulled onto this critical path by the
  unix-family decision rather than waiting for a C frontend. It is a wrapper
  and not `crate-type = ["rlib", "staticlib"]` on slibc itself because cargo
  emits every declared crate type in *one* rustc invocation even for a
  dependency unit: a `no_std` staticlib needs an in-crate `#[panic_handler]`,
  and that handler is then a duplicate `panic_impl` lang item for all 211 of
  userland's rlib uses. The 42 headers under `slibc/include/` are *generated*
  by `slibc/build.rs` from the creation hunk of
  `toolchain/libc/0001-slopos-libc.patch` — the same declarations the compiler
  reads, so a header cannot drift from the export it describes.
- **The `slopos_` thunk layer is gone.** 40 exports existed only so a patched
  PAL could `extern "C"`-declare them — the whole `*at` family, `lseek`,
  `stat`/`lstat`/`fstat`, `readv`/`writev`, `mmap`, `flock`, `utimensat`,
  `uname` — and they carry their real C names. Seven keep the prefix because
  they name calls no libc has: `slopos_spawn_path`, `slopos_getdents64`,
  `slopos_futex_wait`, `slopos_futex_wake`, and the three CPU calls
  (`get_cpu_count`, `get_current_cpu`, `set_cpu_affinity`); the ones whose
  only caller was the deleted PAL are deleted with it, and so are the four
  that had a real C equivalent and no caller at all.
- **Two ABI divergences are closed, by force rather than by choice.** std's
  unix PAL reads `siginfo_t.si_addr` and uses `msghdr`/`cmsghdr`, so `si_addr`
  moved from offset 24 to **16** — into the union arm Linux has, rather than a
  sibling field — `msghdr` became Linux's 56-byte form with a real `*mut iovec`
  that the kernel now walks instead of one inlined descriptor, and `cmsghdr`
  became 16 bytes with `CMSG_DATA` at +16. Both were named as divergences by
  the POSIX-floor section above; both now read the same from a Linux header and
  from `abi/`.
- **`sendmsg`/`recvmsg` stopped borrowing the private range.** They were
  private *because* their layouts were not Linux's, and the gate's allowlist
  said exactly that. With the layouts fixed the reason is false, so they take
  Linux's 46 and 47 and the private range compacts behind them.
- **Swapping the platform layer found three real bugs, and they were in the
  kernel and in slibc rather than in the port.** A patched PAL had been
  hiding each one by choosing a different path:
  - **The kernel could not write into a page that was merely promised.**
    OSTD's user-copy validates the leaf and refuses — it takes no fault — so a
    user buffer was only copyable once its pages were *already* present. Since
    `brk`, `mmap` and `fork` all leave pages absent or COW by design, the first
    `read(2)` into a fresh `Vec` had no present leaf to write and answered
    `EFAULT`; the old PAL never saw it because every kernel→user write in the
    tree targeted eagerly-mapped memory (`exec`'s segments, the signal frame).
    `mm::user_copy` now resolves the range through the ordinary fault path
    first, which is what Linux does by faulting. The ordering is load-bearing
    and cost one debugging session: the populate runs *before* the copy's
    `KArc<VmSpace>` is taken, because the demand path refuses to install a
    page while another reference to the space is live — populating with the
    copy's own handle held spun to its bound and then gave up, and made the
    faulting task's retries look like an address-space reader that was not
    draining.
  - **A task the kernel killed on a fault reported `exited(139)`, not
    `signalled(SIGSEGV)`.** `exit_status_word` keyed on
    `TaskExitReason::Signalled`, and the fatal-fault path records
    `UserFault` — a diagnostic distinction, not a different kind of death.
    The old PAL read `ExitInfo`'s fields directly and papered over it; std
    reads the POSIX status word, where `exit(139)` and death by `SIGSEGV` are
    different answers and a build driver acts on which it got.
  - **`realpath` resolved a relative symlink target against the wrong
    directory.** It stripped a component that had not been appended, so
    `/bin/ls -> coreutils` canonicalised to `/coreutils` and answered
    `ENOENT`. `libc_abi_test` holds that case now.
- **The gate this replaces, replaced.** `patch_std.sh`'s
  `check_arm_precedes_fallback` existed because a `cfg_select!` arm placed
  after the `_` wildcard is dead code that still compiles — it shipped once as
  a `ud2` in `std::process::exit`. That failure mode went with the script. Two
  appear in its place: the fork drifting from `rust-toolchain.toml`, and a
  `libc` struct disagreeing with `abi/`. `scripts/check_toolchain_pin.sh` and
  the const asserts on both sides are what catch them. A third was found by
  building it: `git apply` run inside a work tree resolves a patch's paths
  against the *repository* root, so a sysroot materialised under
  `third_party/` got every path ignored and exit 0 — an unpatched sysroot
  reported as success. Repository discovery is ceilinged above the sysroot and
  every patch is re-checked in reverse, because a gate that cannot see a no-op
  is not a gate.

**What this deliberately did not do.**

- **The triple is still a JSON spec, not a built-in.** Tier 3 buys the built-in
  name, the `--print target-list` entry and the `STAGE0_MISSING_TARGETS` row —
  and ships **no artifacts**, so `-Zbuild-std` stays mandatory either way until
  tier 2, which is where `x86_64-unknown-redox` sits. What a built-in triple
  actually costs is a stage-2 cross toolchain built from the fork, whose own
  dev-guide figure is ~100 GB of build directory, and what it buys over the
  linked sysroot is dropping three flags from one script. The diff a tier-3 PR
  wants — `spec/base/slopos.rs`, `spec/targets/x86_64_unknown_slopos.rs`, the
  `supported_targets!` entry and `platform-support/slopos.md` — is therefore
  written when the PR is, not before it.
- **Upstreaming has not happened.** The two patches *are* the two PRs, in the
  order the tier policy asks for (`rust-lang/libc` before `rust-lang/rust`),
  and the cost is unchanged: a maintainer name on record, the verbatim
  policy-response text, and an `MIT OR Apache-2.0` licence on the contributed
  files. Nothing contributed is GPL'd kernel or slibc code — the libc module
  and the std cfg-sites are new work, and the three `os/slopos/` files derive
  from upstream's own `os/redox/` ones under the same licence.
- **Being a *host* is still gated on proc macros, not on the target spec.**
  `compiler/rustc_driver/Cargo.toml` is `crate-type = ["dylib"]` and
  `rustc_session`'s `invalid_output_for_target` rejects `Dylib`/`ProcMacro`
  outright when `!target.dynamic_linking`, so a static rustc that expands proc
  macros is not a thing that exists today. That is unchanged, and it is
  Workstream 1.2's problem.
- **The three layouts std's unix PAL never reads stay divergent**: the
  truncated `ucontext_t` with its `rt_sigreturn`, the `termios2`-shaped
  `struct termios`, and `NSIG` at 32. They are binary-compatibility work, not
  platform-layer work, and the open decision below is where they belong.

---

## Phase 1 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

This phase is **XL**. **Decided: the Rust toolchain is Rust-hosted** — rustc
with the cranelift backend and a Rust linker, no LLVM. Read that as a statement
about *who compiles Rust*, not about which languages SlopOS supports: declining
LLVM declines a **C++** toolchain port (templates, exceptions, libc++/libc++abi,
the Itanium ABI), which is the expensive part, and says nothing about C.
A C toolchain written in Rust is a separate and wanted track — Workstream 1.4.
The cost of this decision is upstream work: cranelift-only rustc bootstrap does
not currently work (it did in 2020 and regressed), cranelift emits no debug
info, and `wild` is explicitly not production-grade. Redox took the other road —
relibc, GCC, binutils, then rustc in January 2026 on its third attempt — which
is the reference class this decision is *declining*, with eyes open.

### Workstream 1.1 — A Rust codegen path for a `no_std` kernel target (**L**)

Decided pure Rust, so the C floor is out of scope and the risk moves into
cranelift's coverage of *this* tree's kernel target. Spike this first, before
anything else in this phase, because a negative answer changes the decision:
`targets/x86_64-slos.json` requires soft-float with `-sse` and `rustc-abi:
softfloat`, safestack, custom `link_section`s, naked functions, and
`-Zemit-stack-sizes` — the last is what `check_stack_sizes.sh` reads, so a
backend that does not emit `.stack_sizes` silently disarms the S-5 gate. Naked
functions are backend-independent now (emitted as global asm) and inline asm is
largely stable in cg_clif; soft-float, safestack and `.stack_sizes` are
unverified. The linker is the second half: `wild` or a linker written here, and
it must honour `-T link.ld` with the registry sections
`check_registry_sections.sh` polices.

Note the split this permits: the *release* kernel can keep being built by an
LLVM rustc on a host for as long as cranelift's codegen quality matters, while
the self-hosted loop builds the dev kernel. Self-hosting does not have to mean
every artifact is self-built on day one.

### Workstream 1.2 — Dynamic linking is mandatory (**L**)

Not optional, and pure Rust does not dodge it: `slopos-ostd-derive` is a
proc-macro crate (`#[derive(SlotFields)]`) and `paste` is another, and rustc
loads proc-macro crates as host **dylibs** at runtime. Building SlopOS on
SlopOS therefore requires `PT_INTERP` + `dlopen` — today `PT_INTERP` is rejected
(`mm/src/elf.rs`), the target is `relocation-model: static`, and every
binary is fixed at 0x400000. The only escapes are writing an out-of-process
macro server (novel work) or deleting proc-macro use from the workspace. This
also brings dynamic TLS (`__tls_get_addr`, DTV), which does not exist, and an
arbitrary load base: `ET_DYN` is accepted only at `PROCESS_CODE_START_VA`
(`ElfError::UnsupportedLoadBase`), so an interpreter and its executable cannot
both be loaded. That trio — `PT_INTERP`, `dlopen`, a movable base — is also
most of what binary compatibility with prebuilt Linux userland would need, so
the decision below wants settling here rather than separately.

**Where rustc enforces this, exactly.** It is not a convention:
`compiler/rustc_driver/Cargo.toml` hard-codes `crate-type = ["dylib"]`, and
`rustc_session`'s `invalid_output_for_target` returns "invalid" for
`Dylib`/`Cdylib`/`ProcMacro` whenever `!sess.target.dynamic_linking` — which
is what `targets/x86_64-unknown-slopos.json` says today by omission. Upstream
has declined to make `+crt-static` and proc macros coexist (rust-lang/rust
#78210, closed "not intending to fix"), and `-Zdual-proc-macros` solves
cross-*compiling to* SlopOS, not running rustc *on* it, because there the host
is SlopOS. `rustc_metadata` additionally reaches `dlopen` through
`libloading`, whose `unix` module is gated `any(unix, target_os = "motor", …)`
— which the unix-family decision has already satisfied, so what is left owing
here is the loader itself rather than a carve-out upstream.

**One escape is now real rather than novel work.** `-Zwasm-proc-macros`
exists: the flag is in `rustc_session/src/options.rs`, the loader arm is in
`rustc_metadata/src/creader.rs`, the bootstrap key is
`rust.wasm-proc-macros`, and the MCP (compiler-team#1017) was accepted
2026-08-02 with proc macros compiled to `wasm32-wasip2`. The execution engine
is not chosen yet, so this is a bet on upstream rather than a capability to
use today — but it is the one path where a `dynamic_linking: false`,
statically linked in-guest rustc expands proc macros at all, and it costs an
embedded wasm interpreter instead of a dynamic loader.

**The reference class prices the alternative.** Redox needed exactly this
trio and nothing less: the 2017 attempt static-linked rustc's components and
*disabled proc macros*, by 2019 "rustc is no longer capable of running
statically linked", and the answer was a real ELF dynamic linker
(`relibc/src/ld_so`, `ld64.so.1`) plus `dynamic_linking: true`,
`position_independent_executables: true`, `crt_static_respected: true`.
Self-hosting landed January 2026 — eight and a half years after the first
attempt, and the residual bug list is a ready-made checklist: TPOFF
relocations for undefined symbol indices, an allocator mismatch between the
libc and the dynamic linker, `mremap`, and spurious futex wakeups being
mistaken for timeouts.

### Workstream 1.3 — Getting code in and out (**S** for the goal, **M** beyond it)

Off the critical path, and this is a real scope reduction: `Cargo.lock` holds 47
entries of which only nine are third-party (`bitflags gimli libm limine paste
proc-macro2 quote syn unicode-ident unwinding`). Vendoring that is trivial, so
**building SlopOS on SlopOS needs no network at all** — no TLS, no crates.io, no
`git`. Those remain wanted for a general dev machine (there is no TLS anywhere:
`curl` rejects `https://` outright; DNS is one query at a time machine-wide; the
TCP window is capped at 32 KiB by a fixed buffer), but they are Phase 1+
comfort, not a blocker for the goal.

### Workstream 1.4 — A C toolchain, written in Rust (**M**/**L**, not on the critical path)

C is not foreclosed by the pure-Rust decision, and closing it off would be a
mistake: C is the interoperability floor of the world, and every piece of it can
be built in Rust here.

- **A C library.** `slibc` already *is* a C ABI — ~230 `#[unsafe(no_mangle)]`
  Rust functions, now including the `*at` family, `getdents64`, `pread`/`pwrite`,
  `readv`/`writev`, `flock`, `utimensat` and `uname`. What is missing is
  linkability: `crate-type = ["staticlib"]` alongside `rlib`, generated
  `include/*.h`, and a `crt0.o` emitted from the existing `_start` +
  `__slibc_start` pair (`userland/src/lib.rs:20-34`, `slibc/src/crt/mod.rs:97`),
  plus libm (wrap the `libm` crate the tree already vendors for `font/`),
  `setjmp`/`longjmp`, `opendir`, `qsort`, `strerror` and the `<time.h>`
  calendar. **S/M**, and it is the single gate that turns "can a C program be
  built here" from *no* into *yes*.
- **A C frontend.** A C99 compiler written in Rust emitting cranelift IR, reusing
  the *same* backend and the *same* Rust linker as the Rust toolchain — the
  marginal cost is a frontend, not a second toolchain. `saltwater` (formerly
  `rcc`) is the existence proof of exactly this shape, though unmaintained since
  February 2025, so treat it as a reference design rather than a dependency.
  **M** for a C99 frontend that compiles simple, generated, switch-heavy C;
  **L** for one that survives real-world C (GNU extensions, `__builtin_*`,
  bitfields, VLAs, inline asm, `setjmp` interaction).
- **What this buys.** tree-sitter (a ~10 kLoC C99 runtime plus generated
  parsers, which is close to the easiest interesting C target there is) and
  therefore helix; `cc`-crate build scripts across the ecosystem; and every C
  library worth having. `[INFERENCE]` on tree-sitter's exact dialect needs —
  verify against its sources before committing.
- **What it still does not buy.** C++. Templates, exceptions and unwinding,
  name mangling, the Itanium ABI, libc++/libc++abi — an order of magnitude past
  a C frontend, and the reason the LLVM route was priced as it was. Nothing in
  this plan needs C++, and this workstream does not change that.

Order it after Phase 1's Rust loop closes: the C frontend is much cheaper to
write once cranelift and the linker are already known-good on this target.

**Phase 1 exit criteria:** in-guest `cargo build` of this repository's kernel
produces an ELF byte-identical in behaviour to the host build, verified by
booting it.

---

## Phase 2 — Install what you built

**Outcome:** the guest writes a bootable medium and reboots into its own kernel.

Nothing here exists. `write` on a `/dev` block node returns `ReadOnly`
(`fs/src/devfs/mod.rs`) and reading one needs `TASK_FLAG_SYSTEM`; there
is no FAT/vfat support anywhere, so an ESP cannot be written; partition tables
are parse-only (`fs/src/partition.rs`); Limine is fetched and installed by host
scripts; QEMU boots `order=d` (CD only) with throwaway OVMF vars. Needed: a
writable block path, FAT32 write, a bootloader installer or a direct EFI stub, a
`limine.conf` editor, `SYSCALL_REBOOT` (exists) landing on the new image, and
A/B slots with rollback. One prerequisite is already in place: a second disk can
be mounted at an arbitrary path, so the installer has somewhere to read from and
write to. `AGENTS.md`'s QEMU-only execution boundary currently forbids exactly
this operation and needs a scoped exception for the guest's own ESP.

**Phase 2 exit criteria:** `just boot-persist`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 3 — Bare metal (not committed)

Out of scope for the current goal, which ends at Phase 2 in QEMU. Recorded so
the cost is known: no NVMe and no AHCI (virtio-blk is the only storage driver,
so a real machine has no disk); no USB at all, so a laptop without PS/2 has
**no keyboard** (`plans/usb-xhci.md`); PCI is ECAM-only and *panics* without
MCFG; x2APIC is forcibly disabled so machines with APIC IDs > 254 do not boot;
no real NIC; no ACPI SCI/GPE runtime, so no power button, no lid, no thermal
events during a multi-hour build; no CPU frequency management; EFI runtime
services are `ResetSystem` only; COM1 port I/O is the only serial, so the debug
channel and the KTAP transport vanish exactly when bare-metal debugging starts.
The RTC is no longer on this list — there is a CMOS driver, and it is what the
boot step reads first.

---

## Open decisions

- [ ] **Does the ABI become binary-compatible, and does the toolchain then stop
      being a port?** SlopOS is Linux-ABI at the numbers, the constants and
      most of the layouts, which buys *source* compatibility. Running prebuilt
      glibc-linked binaries — the upstream `rustc` among them — needs the
      layouts the POSIX-floor section above still names as divergent (the
      ucontext and `rt_sigreturn`, `struct termios`, `signalfd_siginfo`, and
      `NSIG` 32 → 64, for which glibc reserves signals 32 and 33), plus
      ~70-90 thin entry points and the dynamic-linking trio Workstream 1.2
      already owes. Two of the seven left that list on their own: `si_addr`
      and `msghdr`/`cmsghdr` were closed by the unix-family decision, because
      std's own PAL reads them. The reference
      class says it is reachable: Asterinas runs an unmodified NixOS userland
      on 240+ syscalls of safe Rust with no private calls at all, and gVisor
      runs unmodified binaries with 277 of 351 implemented, because a runtime
      that meets `ENOSYS` probes for a fallback. Decide it *with* Workstream
      1.2. It would *not* retire Workstream 1.1 — the earlier claim that it
      would was wrong. Asterinas is the proof of the ceiling: it is
      binary-compatible to the point of running an unmodified NixOS userland,
      with a real vDSO, `PT_INTERP`, Linux-layout `siginfo_t`/`ucontext_t` and
      procfs, and it is still *always* cross-built from Linux — there is no
      published instance of a Rust compile running inside it. Binary
      compatibility buys running a prebuilt rustc; it does not buy a target
      that can be a host, and it does not remove the proc-macro `dlopen`.
- [ ] **Does the dev root stay attested?** A machine that rewrites `/usr` while
      building itself un-attests exactly the blocks it changes — and now keeps
      them un-attested across host rebuilds, so the count only ever falls.
      Decide which paths stay verified and what `verity=require` asserts for a
      workbench.
- [ ] **How does source get in, now that a second disk can hold it?** The
      cheapest answer was always a second virtio-blk disk carrying the vendored
      tree, and its prerequisite is done: `mount(2)` takes a named device, and
      `just test-capacity` already builds a 16 GiB volume populated from the
      host with this repository and the pinned sysroot. What is left to decide
      is the *workflow*, not the capability — a host-built image refreshed per
      session, a 9p/virtiofs mount, or a plain TCP transfer once there is one.
- [ ] **When does swap arrive, and what chooses the victim?** Not in the
      storage work, deliberately (see above). A build that overcommits currently
      dies at the faulting task with a SIGBUS-coded exit. Decide whether the
      answer is swap plus a reclaim policy, or a per-build memory budget that
      makes overcommit not happen.

**Decided.** Syscall ABI: **Linux x86-64 numbering, one table, a private range
at 1024, and a Linux number obliges the Linux signature.** Rust toolchain:
Rust-hosted (cranelift + a Rust linker), no LLVM
and no C++ toolchain port; time is not the constraint. C is *not* excluded — a
C library and a Rust-written C frontend are Workstream 1.4, off the critical
path. Scope: the full in-guest loop, Phases 1–2, in QEMU; bare metal is not
committed. Identity: single-user, uid 0, permanently — no persistable
principal, so file ownership and a medium-resident quota ledger stay out of
scope and `stat`'s uid/gid fields exist for layout only. Directory scaling: an
in-memory name index, not an on-disk htree, so `e2fsck` stays the oracle for
every image this kernel writes.

**Std platform layer: unix family over a real libc — landed.**
`x86_64-unknown-slopos`, `target-family = ["unix"]`, `env = "slibc"`, a
`libc/src/unix/slopos/` module, and std riding its own `sys/pal/unix` — Redox's
architecture, and the only one that has produced a self-hosting Rust OS.
`slibc/std_pal/` is deleted rather than moved; `slibc` is a real C library
(`staticlib`, generated headers, `crt0.o`), which pulled Workstream 1.4's first
bullet forward; and the `siginfo_t.si_addr` and `msghdr`/`cmsghdr` divergences
are closed, because std's unix PAL reads both. The rejected alternative was a
bespoke PAL over a crates.io ABI crate (Motor OS's shape), which costs six
hard-breaking third-party crates — `libloading` among them, so an in-guest
rustc could not be built at all — as permanent carve-outs. What is *not* done
is the built-in triple: tier 3 ships no artifacts, so `-Zbuild-std` stays
mandatory until tier 2 either way, and the fork is two patches over a linked
sysroot rather than a stage-2 cross toolchain. See "The target is a host's
target".

---

## Touch list (current paths — verify before editing)

- `mm/src/elf.rs` — `PT_INTERP` rejection, the one image cap that is still
  policy rather than plumbing (Phase 1).
- `vt/src/lib.rs`, `terminal-core/src/{input,grid}.rs`,
  `userland/src/apps/terminal/{input,mod}.rs`,
  `userland/src/apps/shell/input.rs`, `font/src/{lib,atlas,boxdraw,bitmap}.rs`,
  `core/src/syscall/font_handlers.rs`, `net-core/src/render.rs` — the terminal
  above. Listed not as work but as what a later phase must not quietly undo:
  the parser's private-marker byte (`>` must not reach the SGR handler), the
  reply queue's drop-whole-answers rule and its drain landing on the same loop
  turn, "a reply is not a cursor movement" (it must not cancel a pending
  autowrap), Shift as the mouse-reporting override, the kernel's Shift+PgUp
  interception requiring Shift *without* Ctrl, `boxdraw`'s single `Geom` (every
  stroke must come from it or cells stop joining), the atlas chunk arithmetic,
  and `is_renderable` mirroring `GLYPH_RANGES` — each is an invariant a change
  nearby can break without failing to compile.
- `shell-core/src/{lexer,syntax,pattern,fields,arith,param,qbuf}.rs`,
  `userland/src/apps/shell/{expand,glob,exec,funcs}.rs` — the shell above.
  Listed not as work but as what a later phase must not quietly undo: the
  lexer's `Incomplete`, `QBuf`'s per-byte quoting, "an unmatched pattern is
  left literal" and "a substitution's output is never re-tokenized" are each
  an invariant a change nearby can break without failing to compile.
- `userland/src/apps/coreutils/`, `userland/src/bin/coreutils.rs`, the
  justfile's `coreutils_tools`, `scripts/build_fs_image.sh`'s symlink loop and
  `scripts/gen_initramfs.py`'s `MODE_LINK` records — the utility set above.
  Listed not as work but as what a later phase must not quietly undo: the
  installed names, the implemented table and `coreutils_test`'s check that they
  agree are three places one utility appears, and `mod.rs`'s `TOOL_SETS` is
  what keeps a new tool to one file.
- `editor-core/src/`, `userland/src/apps/editor/`,
  `appkit/src/{text,paint,run,node,tree,style}.rs`,
  `appkit/src/widgets/{code_view,tree_view,editor_tabs,menu_bar,line_edit,drag_handle,card,icon}.rs`,
  `appkit/src/layout.rs`'s release pass, `windowing/src/clipboard.rs` and
  `userland/src/apps/compositor/mod.rs`'s `protocol_pointer_grab` — the editor
  above. Listed not as work but as
  what a later phase must not quietly undo: positions are characters and only
  `byte_of` converts, a display column is not a character column wherever a tab
  can appear (`Viewport::first_col` is a display column, and `buffer.rs`'s
  `display_col` and `code_view.rs`'s copy of it must agree), a compound edit
  opens one history transaction, the highlight cache
  is invalidated at the *first* edited line and nowhere later, transient
  interaction state (a drag, a click run, a resize) belongs to the application
  because the widget holding it is rebuilt between events, a release reaches
  the widget that latched on the press whether or not it lands inside it and
  whether or not it lands inside the window, a widget answers a key only when it
  holds the focus (nothing else in the tree will decline it),
  `Rect::to_damage_rect`'s bounds
  are inclusive like every other `DamageRect`, and `measure` and
  `paint` must agree on a font size or a click lands on the wrong character.
- `toolchain/PIN`, `toolchain/rust/`, `toolchain/libc/`,
  `scripts/make_slopos_sysroot.sh`, `scripts/check_toolchain_pin.sh`,
  `targets/x86_64-unknown-slopos.json` and `userland/userland.ld:44-50` — the
  std/target/unwinding triangle, a fork now rather than a mutation. Listed not
  as work but as what a later phase must not quietly undo: the materialiser
  must replace `lib/rustlib/src` with a real copy before it patches anything,
  because the clone is hardlinked and patching in place writes through into
  `$RUSTUP_HOME`; `git apply` must run with repository discovery ceilinged
  above the sysroot, or it resolves the patch's paths against *this*
  repository's root, ignores every one of them and exits 0; each patch is then
  re-checked in reverse so that silent no-op cannot pass; and `toolchain/PIN`
  is the only place a patch's hash is written down.
- `abi/src/syscall/numbers.rs`, `core/src/syscall/handlers.rs`,
  `scripts/check_syscall_abi.sh`, `scripts/gates/syscall/` — the Linux number
  table, the two dispatch tables and the gate that holds them to
  `syscall_64.tbl`. Listed not as work but as what a later phase must not
  quietly undo: a number below `SYSCALL_PRIVATE_BASE` means Linux's call of
  that name and nothing else, the private range stays contiguous, and a
  private constant that borrows a Linux syscall's name needs a stated reason
  in the allowlist. The capability histogram counts registered entry points,
  so it still moves when a syscall is added.
- `scripts/qemu_run.sh` — disk attachment, boot order (Phase 2).
- `fs/src/devfs/mod.rs`, `fs/src/partition.rs` — writable block nodes,
  partition writing (Phase 2).
- `AGENTS.md` — the QEMU-only execution boundary, which forbids exactly the
  Phase 2 install operation and needs a scoped exception.
- `fs/src/ext2/dirindex.rs`, `fs/src/ext2/journal.rs`, `fs/src/verity.rs`,
  `drivers/src/virtio_blk.rs`, `fs/src/fsreport.rs` — the storage work above.
  Listed not as work but as what a later phase must not quietly undo: each one
  carries an invariant (index completeness, no allocation on the commit path,
  the trailer's byte layout, the descriptor-ring arithmetic, the report's wire
  form) that a change nearby can break without failing to compile.
