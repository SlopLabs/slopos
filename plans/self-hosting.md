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
compiler bootstrap itself). Three more, a page-fault path that can reach the
device, a POSIX floor a build system can stand on, and a filesystem that can
hold a tree, have landed.

## Architectural constraints (do not violate)

- **Unsafe surface.** Only `slopos-ostd` may use `unsafe`; every other kernel
  crate stays `#![forbid(unsafe_code)]` and `check_unsafe_expansion.sh` sees
  through macros. Nothing in this plan earns an exemption.
- **Allocation discipline.** `KBox`/`KVec`/`KArc`/`KBTreeMap` only. Every
  toolchain-sized buffer this plan touches must become a chunked or page-list
  design rather than a bigger single allocation: `MAX_ALLOC_SIZE` is 1 MiB
  (`mm/src/slab/mod.rs:61`) and raising it is not the fix. The verity hash
  array, the attest bitmap and a ramfs file's body are all chunked for exactly
  this reason and are the pattern to copy.
- **Stack frames ≤ 2 KiB** against a 4 KiB guard page. This is why a 4096-byte
  path lives in a `KVec` — in `CanonPath`, in `UserPath`, and in the shell —
  rather than in an array on a frame, why `NameBuf` borrows from the canonical
  path instead of copying out of it, and why `BlockCache` and `Journal` are
  built with `KBox::try_init` instead of by value.
- **Task ownership I1–I8** and **no `async fn` in a kernel crate**. The
  sleepable fault path this rests on is a blocking task on its own kernel
  stack, not an executor.
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source, ever
  — which rules out lifting busybox-lineage utilities or Linux userland code for
  Phase 1's utilities. Concepts, ABI numbers and struct layouts are free to
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
  on an owned directory descriptor rather than splitting `slopos_list`'s output
  on newlines, so a filename containing a newline is just bytes; `symlink`,
  `read_link`, `hard_link`, `set_permissions`, `File::set_times`,
  `read_vectored`, `write_vectored` and `FileExt::read_at`/`write_at` are real
  instead of `unsupported`; and `Command::spawn` goes through `spawn_path` with
  a cwd in `SpawnAttrs`, so nothing allocates between fork and exec against the
  single global malloc spinlock a multithreaded parent could hand over locked.

**What this deliberately did not do.** The syscall numbering is still bespoke —
the *layouts* are Linux's now, which is the half that carries no design value
and the half a libc port cannot work around, but the numbers are still
append-only SlopOS ones. That is the open decision below, and this work made it
cheaper rather than settling it. Process-group waits (`pid == 0`, `pid < -1`)
are still `ESRCH`, because there is no process-group wait to answer with.
`st_uid`/`st_gid` exist for layout and read 0, which is the single-user
decision below, not an omission.

Four smaller divergences, stated rather than hidden:

- **The cwd is per-thread.** `CLONE_FS` is accepted and ignored, so a `chdir`
  is visible only to the thread that made it, where POSIX has the cwd per
  process. The buffer is a `TaskOwnCell` whose whole contract is that only its
  owning task reads or writes it, so sharing it means a lock, a lock class and
  a changed signature at every reader.
- **`getdents64`'s `d_name` sits at offset 24, not Linux's 19.** The record
  header is naturally aligned rather than packed, which every in-tree consumer
  and its asserts agree on, but it is a layout a libc port compiled against a
  real `struct linux_dirent64` cannot work around — the one place the
  layouts-are-Linux's claim above does not hold.
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

## Phase 1 — A workbench you can type in

**Outcome:** you can edit a file, search a tree, run a script, and read the
output — without a Linux host.

### Workstream 1.1 — Utilities that are executables (**M**)

`ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `diff`, `env`, `ps` exist only as shell
builtins (`userland/src/apps/shell/builtins/`); `/bin` holds 17 GUI and network
binaries. Anything that spawns a tool directly gets `ENOENT`. Give the existing
builtins `main`s, then write the absent set: `grep` `find` `sed` `sort` `uniq`
`tr` `cut` `xargs` `which` `test`/`[` `printf` `basename` `dirname` `mktemp`
`tar` `gzip` `patch` `cmp` `install` `sha256sum` `nproc` `stty` `less`. Fix the
semantics that are wrong rather than missing: `rm`/`cp` have no `-r`, `mkdir`
has no `-p`, `diff` cannot produce a patch. (`sleep`'s unit, `kill`'s
signal argument and `date`'s clock were blockers for the POSIX floor above and
are done.)

### Workstream 1.2 — A shell that can drive a build (**L**)

No `if`/`while`/`for`/`case`/functions, no command substitution, no here-docs,
no globbing (`userland/src/apps/shell/`). Structural caps: 8 pipeline stages,
64 argv words. Path widths and job control are no longer among them: the
shell's buffers are `USER_PATH_MAX`-sized and heap-backed, and `fg`/`bg` can
resume a stopped job.

### Workstream 1.3 — A terminal an editor can use (**M**)

`encode_key` emits arrows, Home, End and Delete only
(`terminal-core/src/input.rs:230-282`): no F1–F12 (the keycodes exist and are
dropped), no Alt-prefixing, no modified arrows, no `CSI Z`, and PageUp/PageDown
never reach the PTY. No mouse reporting, no DA/DSR replies. The font atlas
covers ASCII + Latin-1, so box-drawing and non-Latin source render as diamonds.

### Workstream 1.4 — An editor (**M**)

Write one — not because C is foreclosed (it is not; see Workstream 2.6), but
because nothing upstream is reachable *before* a C frontend exists, and because
an editor is where a desktop OS earns its character. Highlighting does not have
to wait for C either: `syntect` with the pure-Rust `fancy-regex` backend is a
Rust-only path to TextMate grammars. Start against the existing terminal; the
GUI version needs a real multi-line text widget, which `appkit` does not have (a
single-line `text_field`, and a byte-oriented text API). helix comes back onto
the table once 2.6 compiles tree-sitter.

**Zed is not a roadmap item.** It needs wgpu → Vulkan (no GPU driver, and the
Vulkan loader is itself a `dlopen` ICD architecture), tree-sitter, a live C++
dependency set, and a build performed by a toolchain that does not exist yet.
Every one of those is a separate multi-month project whose payoff is one editor.

**Phase 1 exit criteria:** a shell script in the guest checks out, greps,
edits and archives a source tree, driven from a terminal running a native
editor.

---

## Phase 2 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

This phase is **XL**. **Decided: the Rust toolchain is Rust-hosted** — rustc
with the cranelift backend and a Rust linker, no LLVM. Read that as a statement
about *who compiles Rust*, not about which languages SlopOS supports: declining
LLVM declines a **C++** toolchain port (templates, exceptions, libc++/libc++abi,
the Itanium ABI), which is the expensive part, and says nothing about C.
A C toolchain written in Rust is a separate and wanted track — Workstream 2.6.
The cost of this decision is upstream work: cranelift-only rustc bootstrap does
not currently work (it did in 2020 and regressed), cranelift emits no debug
info, and `wild` is explicitly not production-grade. Redox took the other road —
relibc, GCC, binutils, then rustc in January 2026 on its third attempt — which
is the reference class this decision is *declining*, with eyes open.

### Workstream 2.1 — The ABI question (still open — see Open decisions)

SlopOS's numbering is bespoke and append-only (`yield=0, exit=1, write=2,
read=3`, `abi/src/syscall/numbers.rs`) while the *constants and layouts inside*
the calls are Linux's — errno, `O_*`, `PROT_*`, `MAP_*`, `CLONE_*`, `AT_*`,
`FUTEX_*`, termios ioctls, `struct stat`, `struct timespec`, `struct flock`,
`struct iovec`, `struct dirent64`, `stack_t`, `siginfo_t`, and the wait-status
encoding. The POSIX-floor work paid for the layout half; what remains bespoke is
the numbering. The two reference designs split on architecture, not taste:

- **Asterinas** — the framekernel whose AD-1/AD-2 discipline this tree already
  follows — is **Linux ABI-compatible by construction**: 210+ Linux syscalls,
  Linux numbers, Linux struct layouts, implemented entirely in *safe* Rust on
  OSTD, with a 14% memory-safety TCB and LMbench parity. Being Linux-ABI does
  not make a kernel a sloppy Linux; Asterinas is the standing proof, and it is
  the same architecture class as SlopOS.
- **Redox** is a microkernel and deliberately *not* Linux-ABI: its kernel
  interface is intentionally unstable and minimal (Plan 9 schemes), and POSIX
  lives in userspace in relibc/redox-rt. The stable ABI boundary is in
  userspace. The result is source compatibility, not binary compatibility —
  every port is a source port, which is precisely why rustc took years.

SlopOS is a framekernel, not a microkernel: services live in the kernel, in one
address space, behind one syscall table. That is Asterinas's shape, and it is
the shape for which a Linux ABI is cheap. What is bespoke here is now *numbering
alone* — the part of an ABI that carries no design value at all.
`AGENTS.md` already settles the licensing half: "ABI numbers,
`errno` values, ioctl codes, struct layouts … carry no copyright, which is why
the ABI-compatibility work is sound."

The counterweight is real and must be priced: ~215 slots becomes ~350, the
capability classification that `core/src/syscall/handlers.rs` proves total has
to cover all of them, and Linux's warts (32 signals, ioctl numbering) become
permanent — though the wait-status encoding and `stat` padding are already here
and already load-bearing. Nothing about adopting the interface obliges adopting
Linux's implementation, architecture or policy — the framekernel quarantine, the
capability authority, the Verus proofs, the ratchets and the retractable
filesystem are all things the ABI cannot touch.

### Workstream 2.2 — A target that can be a host (**L**)

A JSON target can never be a rustc host. `scripts/patch_std.sh` is 715 lines of
sed/perl that mutates the *live rustup sysroot's* std sources in place — an
excellent bootstrap hack and a non-self-hostable one. A native rustc needs
`x86_64-unknown-slopos` compiled into `rustc_target`, a `libc` crate port, and
std upstreamed or carried in a pinned fork. That also kills `restricted_std`,
which currently forces `#![feature(restricted_std)]` into 59 files and makes
every unmodified crates.io crate uncompilable.

### Workstream 2.3 — A Rust codegen path for a `no_std` kernel target (**L**)

Decided pure Rust, so the C floor is out of scope and the risk moves into
cranelift's coverage of *this* tree's kernel target. Spike this first, before
anything else in Phase 2, because a negative answer changes the decision:
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

### Workstream 2.4 — Dynamic linking is mandatory (**L**)

Not optional, and pure Rust does not dodge it: `slopos-ostd-derive` is a
proc-macro crate (`#[derive(SlotFields)]`) and `paste` is another, and rustc
loads proc-macro crates as host **dylibs** at runtime. Building SlopOS on
SlopOS therefore requires `PT_INTERP` + `dlopen` — today `PT_INTERP` is rejected
(`mm/src/elf.rs`), the target is `relocation-model: static`, and every
binary is fixed at 0x400000. The only escapes are writing an out-of-process
macro server (novel work) or deleting proc-macro use from the workspace. This
also brings dynamic TLS (`__tls_get_addr`, DTV), which does not exist.

### Workstream 2.5 — Getting code in and out (**S** for the goal, **M** beyond it)

Off the critical path, and this is a real scope reduction: `Cargo.lock` holds 47
entries of which only nine are third-party (`bitflags gimli libm limine paste
proc-macro2 quote syn unicode-ident unwinding`). Vendoring that is trivial, so
**building SlopOS on SlopOS needs no network at all** — no TLS, no crates.io, no
`git`. Those remain wanted for a general dev machine (there is no TLS anywhere:
`curl` rejects `https://` outright; DNS is one query at a time machine-wide; the
TCP window is capped at 32 KiB by a fixed buffer), but they are Phase 2+
comfort, not a blocker for the goal.

### Workstream 2.6 — A C toolchain, written in Rust (**M**/**L**, not on the critical path)

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

Order it after Phase 2's Rust loop closes: the C frontend is much cheaper to
write once cranelift and the linker are already known-good on this target.

**Phase 2 exit criteria:** in-guest `cargo build` of this repository's kernel
produces an ELF byte-identical in behaviour to the host build, verified by
booting it.

---

## Phase 3 — Install what you built

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

**Phase 3 exit criteria:** `just boot-persist`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 4 — Bare metal (not committed)

Out of scope for the current goal, which ends at Phase 3 in QEMU. Recorded so
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

- [ ] **Linux ABI: adopt the numbering, or stay bespoke?** The one decision
      still open, and the highest-leverage one here. See Workstream 2.1 for the
      Asterinas/Redox evidence. The POSIX-floor work narrowed it: every struct
      layout a libc port cannot work around is already Linux's, so what is left
      to decide is the number table alone. Recommendation: **renumber once, now,
      onto Linux numbers, as the single syscall table** — not a second surface.
      The userland is entirely first-party and rebuilt from source every build,
      so renumbering is nearly free today and compounds in cost with every
      binary written against the current numbers. SlopOS-only calls (SlopRing
      ops, seat, W/L, fate) go in a private high range exactly as Linux does for
      its own extensions. What SlopOS keeps is everything that actually makes it
      not-Linux: the framekernel quarantine, capability authority per syscall,
      Verus proofs, KernMiri, the ratchets, the retractable filesystem.
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

**Decided.** Rust toolchain: Rust-hosted (cranelift + a Rust linker), no LLVM
and no C++ toolchain port; time is not the constraint. C is *not* excluded — a
C library and a Rust-written C frontend are Workstream 2.6, off the critical
path. Scope: the full in-guest loop, Phases 1–3, in QEMU; bare metal is not
committed. Identity: single-user, uid 0, permanently — no persistable
principal, so file ownership and a medium-resident quota ledger stay out of
scope and `stat`'s uid/gid fields exist for layout only. Directory scaling: an
in-memory name index, not an on-disk htree, so `e2fsck` stays the oracle for
every image this kernel writes.

---

## Touch list (current paths — verify before editing)

- `mm/src/elf.rs` — `PT_INTERP` rejection, the one image cap that is still
  policy rather than plumbing (Phase 2).
- `userland/src/apps/shell/`, `terminal-core/src/input.rs:230-282`,
  `font/src/lib.rs:29-48` — shell grammar, key encoding, glyph coverage
  (Phase 1).
- `scripts/patch_std.sh`, `targets/x86_64-slos-userland.json`,
  `userland/userland.ld:44-50` — the std/target/unwinding triangle (Phase 2).
- `abi/src/syscall/numbers.rs`, `core/src/syscall/handlers.rs` — the bespoke
  number table and the capability histogram a renumbering would move
  (Phase 2, and the open decision above).
- `scripts/qemu_run.sh` — disk attachment, boot order (Phase 3).
- `fs/src/devfs/mod.rs`, `fs/src/partition.rs` — writable block nodes,
  partition writing (Phase 3).
- `AGENTS.md` — the QEMU-only execution boundary, which forbids exactly the
  Phase 3 install operation and needs a scoped exception.
- `fs/src/ext2/dirindex.rs`, `fs/src/ext2/journal.rs`, `fs/src/verity.rs`,
  `drivers/src/virtio_blk.rs`, `fs/src/fsreport.rs` — the storage work above.
  Listed not as work but as what a later phase must not quietly undo: each one
  carries an invariant (index completeness, no allocation on the commit path,
  the trailer's byte layout, the descriptor-ring arithmetic, the report's wire
  form) that a change nearby can break without failing to compile.
