# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

Read that sentence precisely, because one word in it is the difference between
Phase 2 and Phase 5: the compiler must *run* here, not be *built* here. A C++
runtime on SlopOS is what lets a cross-built LLVM run; a C++ compiler on SlopOS
is what would let LLVM be rebuilt, and that is an order of magnitude further
out and deliberately not committed.

Nothing in the tree states this goal today. This document is the anchor; the
phases below are ordered by what physically blocks the next measurement, not by
appetite.

**Scale, from the host, measured:** the pinned toolchain sysroot is 1.1 GB;
`librustc_driver.so` is a single 161 MB shared object loaded through
`PT_INTERP`, and `libLLVM.so` beside it is another 208 MB; `builddir/target`
holds 52 GB across 219,895 files. Against that, SlopOS now runs a 24 MiB
executable with a gigabyte of anonymous address space and a demand-paged file
mapping, resolves a 4096-byte path with symlinks in it, stats a file for a real
mtime, mounts a 16 GiB volume holding a million inodes — this repository and
that sysroot among them — and runs a `PT_INTERP` executable that `dlopen`s a
shared object, and throws a C++ exception out of one `dlopen`ed object into
the program that loaded it. What is left is the toolchain itself, and every
constant that produced the storage gap was chosen correctly for an appliance.

**The theme of this plan:** SlopOS's limits are not architectural mistakes,
they are appliance-sized constants and appliance-sized policies. A workbench
needs those quantities derived from the medium (image size, RAM, file size)
instead of frozen at values that fit a test fixture. The work is mostly
*widening under proof*, not redesign, and one exception remains: the compiler
bootstrap itself. The twelve sections between here and Phase 1 are what has
landed, each stating the constraints a later phase must not disturb.


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
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source,
  ever: concepts, ABI numbers and struct layouts are free to take, prose and
  implementation are not. That ruled out busybox-lineage utilities and
  busybox `ash`, so the multicall *shape* and the POSIX Shell Command Language
  (IEEE Std 1003.1 §2.3–2.14) were the inputs and every line was written here.
  Anything new linked into a shipped binary needs a `NOTICE.md` entry; fonts
  stay runtime-loaded. The C++ runtime cost nothing here beyond its entry:
  `libc++` is Apache-2.0-with-LLVM-exception, which GPL-3.0-or-later accepts.
- **Ratchets are measurements, not numbers.** Every phase here grows the stack,
  quota, lockdep, test-count and filesystem-cost pools. Re-measure with each
  gate's `--emit-allowlist` in the same commit and say which change added the
  delta.
- **`just boot`'s `verity=require` keeps meaning what it says.** The shipped
  image stays v1-verified and read-only. Everything this plan makes writable is
  a different medium.

---

## The loop this plan starts from

`just boot-persist` is a machine whose state survives a rude QEMU exit, and no
host build destroys guest data. Every measurement below depends on it: a root
that silently reverts to RAM makes each of them a measurement of the
initramfs. `AGENTS.md` documents the mechanism; what a later phase must not
disturb is four properties of it.

- **The image is marked clean while it is idle**, and re-stamped dirty before
  the next mutation reaches the device — ext4's freeze/thaw ordering, reached
  automatically rather than through `fsfreeze`.
  `test_ext2_clean_stamp_thaws_before_the_next_write` holds it to the offset of
  the first device write.
- **The host refuses rather than rebuilds.** A `PRESERVE_FS_IMAGE=1` image must
  be sound *and* clean (`e2fsck -fn` alone exits 0 on a dirty superblock); one
  that is not stops the build naming its repair. `just boot-persist-reset` is
  the only path that deletes one, and growth goes through `resize2fs` so a
  failed resize leaves the image byte-identical.
- **A rebuild does not re-bless guest writes.** `gen_verity.py` AND-s the old
  attested bitmap into the new one.
- **Image size is bounded by RAM, not by one allocation.** The verity hash is
  chunked, and a mount that cannot hold it answers `VerityError::TooLarge`
  rather than discovering an allocation failure. 512M persist against a 32M
  shipped image.

---

## A large program runs

The second thing this plan rests on, and the reason the phases below are
*measurable*: a toolchain-sized process can start. `bigprog_test` is the
standing proof — a 24 MiB binary that loads intact, maps a gigabyte of
anonymous space, maps a file past a ragged EOF, recurses through three
megabytes of stack and forks with 192 MiB resident. Every one was a hard
refusal before; the last two are sized past the old fixed stack and past the
~170 MiB at which `fork`'s single-`KVec` snapshot used to exceed the 1 MiB slab
ceiling and panic. The suite's guest is 1 GiB to afford that residency.

What it rests on, in case a later phase disturbs it:

- **A user page fault can reach the device.** #PF has no IST entry, so a
  user-mode fault lands on the faulting task's own kernel stack via `TSS.RSP0`
  and takes no preempt hold; the handler re-enables interrupts, leaves
  interrupt-nesting context and resolves. That is what makes a blocking
  `fs.read` legal on the fault path, as a *blocking task* rather than an
  executor. The cost is stated: a kernel #PF with no room for its frame
  escalates to #DF, which `exception_double_fault` classifies against the
  guard pages.
- **The per-process lock is not held across the I/O.** A file-backed fault
  plans under the lock, drops it, reads one page, then re-takes it and
  re-validates that the region still names the same file page. A mapping
  replaced mid-read is a retry, not a page from the wrong file.
- **`exec` stages a header, not an image.** `ELF_HEADER_WINDOW` (7232 bytes),
  every segment extent validated against the file's real length, then the file
  streamed into the mapping in `EXEC_READ_CHUNK` pieces with the lock dropped
  — so kernel memory per `exec` is independent of binary size and
  `EXEC_MAX_ELF_SIZE` (512 MiB) is reachable. The mapping is still eager, so
  `MAX_TOTAL_ZERO_FILL_SIZE` caps the `p_memsz`-past-`p_filesz` part at
  256 MiB, or a one-page ELF declaring a 2 GiB `PT_LOAD` would have an
  unprivileged `exec` memset half a million frames under the lock. The
  whole-file-staging relocation pass is gone; such an image is refused
  (`ElfError::UnsupportedLoadBase`) rather than loaded unrelocated.
- **Mappings are lazy, and the ledger says which are resident.** `brk`, `mmap`
  and both file-mapping modes install no PTE. The stack is 1 MiB inside an
  8 MiB lazy extent over a guard gap. `Pages` is 4 GiB of VA per process and
  still means `RLIMIT_AS`; `ResidentPages` counts present user leaves,
  maintained by the page-table cursor because that is the only place one
  appears. ~113 000 resident against ~950 000 mapped is demand paging working.
  Page tables are charged, so a multi-gigabyte address space is not free.
- **`fork` fails rather than panicking.** The PTE snapshot is chunked at
  `CLONE_CHUNK_PAGES` and every push fallible. `VmaMap::remove_range` and
  `drain` are allocation-free because `munmap` and teardown have no failure
  channel. Fork stays O(resident) under one hold of the parent's lock: a
  parent whose threads could write between the COW mark and the child's
  mapping would not be handing over a snapshot.
- **`mprotect` splits.** A sub-range no longer rewrites its whole VMA's
  protection. A file-backed VMA rebases its offset on every split, or a fault
  in the tail reads the wrong page, and `can_merge_before` compares those
  offsets so restoring a protection re-merges.
- **`exec`'s argument surface is byte-bounded.** `EXEC_MAX_ARG_PAGES` is the
  128 KiB the retired 32-argument cap implied; `EXEC_MAX_ARG_STRINGS` is only a
  loop bound so a NULL-less array terminates.
- **The filemap caps come from the medium.** 128 inodes and a quarter of usable
  frames (floor 1024), because a page under a live user PTE is unreclaimable.
  A page whose start is past EOF is refused; one straddling EOF is zero-filled.

**Two clauses did not land.** There is no OOM *disposition* — a fault that
cannot find a frame kills the faulter with a SIGBUS-coded exit — and choosing a
victim is a policy subsystem that belongs with swap, which also did not land.
User mappings are all 4 KiB; the 2 MiB leaf the page tables support is a
throughput item, not a capability one.

---

## A build system's floor is in place

The third thing this plan rests on: a program can find its files, learn whether
they changed, spawn children and know how they died. `buildctl_test` is the
standing proof — an in-guest build driver that builds a source tree under a
relative path with a symlinked include directory, spawns a stub compiler per
file with `Command::current_dir` set, fingerprints by `mtime` and recompiles
exactly the one input it touched, reads a child's real exit code, tells a
`SIGSEGV` from exit 139, and holds a `flock` a second attempt cannot take.
Every one was a wrong answer before.

What it rests on, in case a later phase disturbs it:

- **A path is 4096 bytes and can contain a symlink.** `MAX_PATH_LEN` is
  `PATH_MAX`, `MAX_NAME_LEN` ext2's 255 — what a `libcore-<hash>.rlib` needs —
  and `MAX_SYMLINK_FOLLOWS` (40) is the budget for the whole resolution. `..`
  pops the resolved-ancestor stack rather than being folded lexically, the only
  way it can mean what POSIX says once a component can be a symlink.
  `RESOLVE_NOFOLLOW_FINAL` makes `lstat` expressible; `RESOLVE_MUST_BE_DIR`
  makes `open("file/")` the `ENOTDIR` POSIX requires.
- **No path is a stack frame.** A 4096-byte array on a 2 KiB frame steps clean
  over the guard page, which no allowlist can raise, so `CanonPath` and
  `UserPath` are heap-backed — Linux's `getname()` — and answer
  `ENAMETOOLONG` rather than truncating into another file's name.
- **Relative paths resolve against the caller's cwd**, including `exec` and
  `spawn_path`, because a `Command` with a relative program and a
  `current_dir` is the shape a build driver has. `chdir` stores the *walked*
  path; an `*at` call re-checks that its base still names the descriptor's
  inode and answers `ESTALE` if not, which is the race that family exists to
  be immune to.
- **`stat` is the Linux `struct stat`**, 144 bytes field-for-field with every
  hole named, because `copy_to_user` copies raw bytes. One producer and one
  type mapping, which is what closed `fstat` reporting a regular file as
  `FS_TYPE_DIRECTORY`.
- **The wall clock is real and settable.** CMOS RTC with Limine's date as
  fallback; neither answering leaves `realtime_ns()` `None`, which ext2 needs
  to decline to stamp rather than claim 1970. `clock_settime` is gated on
  `Capability::Clock`, not `Power`. Cargo's fingerprint model is mtime-based.
- **`waitpid` is POSIX** — `(pid, status, options)`, status written as
  `(code<<8)|sig`. The kernel used to read that pointer as flags and never
  write it, so **every failed compiler reported success**. `WNOHANG` with a
  live child returns 0, which no longer collides with a child that exited 0.
- **A stop is a state, not a dropped bit.** `TaskStatus::Stopped`, group stop
  and continue, no runqueue position, not reapable, still parenting; an
  executing member parks at its next return-to-user boundary rather than being
  descheduled mid-syscall from another CPU. That is what makes `fg`/`bg` and
  Ctrl-Z work. `kill` fans out over the thread group, and `exit_group` exists
  because `exit` is right for a thread and wrong for a process.
- **A fault can be caught.** `SIGSEGV`/`SIGBUS`/`SIGILL` with `si_code` and
  `si_addr`; the default disposition still kills. `SignalFrame` must stay
  immediately above the restorer word, because both restorer trampolines
  depend on RSP pointing at it, and `MINSIGSTKSZ` is pinned to the real frame
  total by a const assert. An unwritable frame push kills immediately for a
  fault signal — its instruction would re-execute and fault again — and on the
  second consecutive failure otherwise. slibc maps a `PROT_NONE` guard below
  every thread stack so std can tell overflow from an ordinary `SIGSEGV`.
- **Threads share what POSIX says they share.** `CLONE_SIGHAND` was validated
  and ignored; the action table is a `KArc<SigHandTable>`. The futex decodes
  `op & FUTEX_CMD_MASK`, so `FUTEX_PRIVATE_FLAG` — set by every std-shaped
  caller, and formerly `ENOSYS` — works, with real timeouts and the bitset and
  requeue forms.
- **`std` reaches the syscalls that exist**: `read_dir` over `getdents64` on an
  owned descriptor so a filename with a newline is just bytes; the link, times
  and vectored calls are real rather than `unsupported`; and `Command::spawn`
  allocates nothing between fork and exec, against a malloc spinlock a
  multithreaded parent could hand over locked.
- **The ABI is Linux's, numbers included.** A number below
  `SYSCALL_PRIVATE_BASE` is Linux x86-64's for the call of that name and
  carries its signature — 114 of them. The 37 operations Linux has no name for
  sit in a private range based at 1024, ARM's `__ARM_NR_BASE` discipline,
  clear of the allocated space and `__X32_SYSCALL_BIT`. Nothing borrows a
  Linux number for a shape it does not implement and nothing keeps a private
  one once the shape agrees. `scripts/check_syscall_abi.sh` holds all of it to
  `syscall_64.tbl`.

**What this deliberately did not do.** The layouts are Linux's except below,
which is the line between source compatibility — this — and binary
compatibility, the open decision further down. Process-group waits are
`ESRCH`; `st_uid`/`st_gid` exist for layout and read 0 (the single-user
decision); `wait4` refuses a non-null `rusage` because there is no per-task
accounting to report. Four divergences, stated rather than hidden:

- **The cwd is per-thread.** `CLONE_FS` is accepted and ignored. The buffer is
  a `TaskOwnCell` whose whole contract is single-owner access, so sharing it
  costs a lock, a lock class and a changed signature at every reader.
- **Five layouts are not Linux's**, and they are the ones a Linux-header binary
  could not work around: `getdents64`'s `d_name` at offset 24 (naturally
  aligned, hidden from C callers by slibc's `readdir`); a truncated
  `ucontext_t` whose `rt_sigreturn` restores from `SignalFrame`, so a handler
  cannot redirect execution through `uc_mcontext`; `NSIG` 32, so no realtime
  signals and no glibc thread cancellation; `termios2`-shaped `struct termios`;
  and a 16-byte `signalfd_siginfo`.
- **Advisory locks are 128 rows machine-wide**, shared by `flock(2)` and record
  locks because they contend on the same file; a principal's share is bounded
  and one holding no lock can always take one. Deadlock detection is the
  self-conflict only, so a two-process cycle parks where Linux says `EDEADLK`.
- **Shared futexes are private-only.** The key is `(address space, address)`; a
  genuinely shared one needs the key to name the backing page.

---

## Storage holds a tree, and a write costs what it writes

The fourth thing this plan rests on: a volume two orders of magnitude past the
appliance root mounts, holds a working tree, and takes a write at a cost
proportional to the bytes it moves. `just test-capacity` is the standing proof
— a 16 GiB ext2 volume carrying a checked-out copy of this repository *and* the
pinned toolchain sysroot, which the guest mounts, walks, searches and writes,
and which `e2fsck -fn` then accepts clean. `scripts/check_fs_throughput.sh` is
the ratchet over the `FSPERF`/`FSCAP` report lines.

Measured: 5 445 files and 1.1 GB in 1 207 directories walked in 1 634 device
reads; a mount 26–27 reads, geometry plus the journal's block map rather than a
sweep; the last of 4 000 names in one directory 0 block reads warm. On the
appliance root, 2 MiB through the real `write(2)` path is 10 transactions, 44
write requests and 10 barriers — against 512 and 578 before this work.

What it rests on, in case a later phase disturbs it:

- **Every appliance-sized constant is derived from the medium.** The block
  cache's capacity comes from the volume's group count at mount, clamped
  `[512, 8192]`, because a group's bitmaps and descriptor table are what every
  allocation re-reads; the log is 1/64 of the image, floored at 4M, capped at
  64M; the verity hash and attest bitmap are chunked at 256 KiB and streamed
  against a running CRC. The ramfs derives its per-file ceiling from usable
  memory with the old 16 MiB as a *floor* and page-chunks its bodies — without
  which the derived ceiling was a lie, since one `KVec` body made the real
  limit `MAX_ALLOC_SIZE` and `/tmp` refused a 1 MiB object file.
- **A miss is O(1) and an insert is not O(n²).** An intrusive LRU that prefers
  a non-bitmap victim; a chained hash index over the journal's slot arrays,
  preallocated at attach because **a commit must not allocate**; and a bounded
  directory name index with a free-space hint — 1 600 255-byte names into one
  directory used to visit 853 333 directory blocks and now costs 2 device
  reads. The hint alone was not enough, so each carries a *proof*, the size no
  block below it has slack for, which is what stops the skip turning an insert
  that would have fitted into a new block.
- **The directory index is a cache, and two rules keep it correct.** A hit is a
  *candidate* — the record is read and its name compared, so a stale entry
  resolves to nothing. The dangerous direction is a complete index missing a
  name, so `complete` is granted only by a walk that reached the end with every
  record filed, and anything that cannot maintain it drops the table; a failed
  transaction drops the index of every directory inode it touched, because
  `Ext2Txn::drop` restores block contents the index would still describe.
- **A write is one transaction per 256 KiB and one request per run of blocks.**
  `IO_FILE_BATCH_SIZE` is deliberately not `IO_STAGING_SIZE`, which stays 4 KiB
  because for a tty or a pipe that number is a latency decision. 256 KiB is
  above the point where data is written home and barriered once instead of
  logged, and under the cache so a batch cannot evict its own blocks. The user
  copy stays *outside* the mount lock and must: the file-fault path runs
  PROCESS_VMS → drop → filemap → `CACHED_EXT2`, and copying under the mount
  lock closes that cycle. Ordered writeback survives gathering because a run
  holds only this operation's dirty data and the commit record is still a
  separate write after every payload.
- **The check point is the lock owner's, not the operation's.** An unbounded
  whole-filesystem `sync()` used to run *inside* the mount lock, so every path
  walk queued behind it; the decision moved to `Ext2Mount::with_fs`, which
  drives the chunked `sync_step` with the lock given back between steps. The
  log's low-water mark scales with capacity, or a batched transaction fails
  with `NoSpace` on exactly the log size meant to help it.
- **The block layer is four requests deep and 32 KiB wide.** Each slot's pages
  are allocated once at probe so the steady state allocates nothing, and the
  descriptor arithmetic is asserted because `DEFAULT_QUEUE_SIZE` is 64 and
  shared with virtio-net and virtio-gpu. A timed-out chain's pages move to
  quarantine, so a stall costs memory and a log line rather than one of eight
  slots permanently.
- **There is more than one filesystem.** The statics that *were* the one ext2
  instance are fields of an `Ext2Mount`, four of them pooled with **four
  separate `lock_class!` sites** — one per instance, because a path walk
  crossing a mount holds one mount's lock while taking the next one's and a
  shared class would make legal nesting look like an unordered self-nest.
  `umount` of an instance's last mount flushes, marks clean and returns the
  slot, which is what gives the write claim back; a leaked claim answers
  `AlreadyClaimed` forever, and that is what the remount test catches. The
  reclaim hook stays `try_lock` because waiting there blocks on the I/O that
  needs the memory.
- **The cost is measured, and the measurement is a gate.** Counters reported at
  the phase boundary rather than from inside the measuring test, because a
  passing test's klog is not on the wire at the verbosity CI grades. Per-MiB
  counts carry caps because they are deterministic for one ISO; throughput is
  graded only against the same run's raw block-device rate, the one quantity
  invariant under a change of accelerator.

**What this deliberately did not do.**

- **No swap, and still no OOM disposition.** Swap is a subsystem — backing
  store, PTE encoding, reclaim policy, victim choice — and belongs with
  whichever phase first needs a build to survive overcommit, not smuggled into
  a storage-sizing change.
- **No on-disk htree.** Directory scaling is the in-memory index, so the format
  stays linear ext2 and `e2fsck` stays the oracle: a lookup after a mount pays
  one scan, four directories are indexed at a time, one past the name cap falls
  back to scanning. The case that *had* to be handled is `EXT2_INDEX_FL`, whose
  index hides inside records that look free — a directory a Linux host indexed
  is **de-indexed** on its first mutation rather than corrupted.
- **Mutations serialise per mount, not per inode.** Two mounts proceed
  independently, which is what `-j16` across `/` and `/home` buys; two writers
  to one filesystem still queue, on bounded waits.
- **A write is still ~15 % of the raw device's rate**, and the gap is requests
  and barriers rather than bytes: 4 624 sectors for 4 096 of data is 1.13x
  amplification, while the reference write issues no barriers at all.

---

## The utilities are executables

The fifth thing this plan rests on: a program that is not the shell can run a
utility. `coreutils_test` is the standing proof — it spawns `/bin/<tool>` by
path, reads what it produced and checks the status. Before this, `ls`, `cat`,
`cp`, `mv`, `rm`, `mkdir`, `diff`, `env` and `ps` existed only as functions
inside the shell, so anything that spawned one got `ENOENT`.

**54 names in `/bin`, one 939 KB binary.** `/bin/coreutils` is a multicall
binary and each name is a symlink to it, so `argv[0]` selects the utility —
busybox's, toybox's and uutils's shape. The alternative was measured: the
smallest SlopOS binary is 158 KB of std, slibc and unwinder before a line of
its own code, so 54 of them would be ~8 MiB of a 32 MiB root for no behaviour.
Each name instead costs one inode and no block (a *fast* symlink, target inside
`i_block`), and the initramfs carries the same set as `S_IFLNK` records, which
is why the utest passes unchanged under `root=initramfs`. `coreutils --list`
plus `coreutils_test` is what stops the installed and implemented sets
drifting.

What it rests on, in case a later phase disturbs it:

- **There is one implementation of each utility, not two.** The shell keeps
  only what changes the shell itself, plus the six POSIX resolves without a
  fork — `echo`, `printf`, `test`, `[`, `true`, `false` — which *delegate into
  the same functions* the `/bin` names run. Everything else goes through
  `PATH`, which is also what gives correct job control: a Ctrl-C reaches a
  forked `yes` and could never reach an in-process one.
- **A utility writes to a `Sink`, never to fd 1.** That is what lets one
  implementation serve both callers: the shell `dup2`s a `>` target onto fd 1
  around a builtin and restores its own, so `echo hi > f` is the same code path
  as `echo hi`. The sink carries whether its destination is a terminal, which
  is why `ls` can column and colour on a tty and emit bare names into a pipe —
  the old builtin printed `name (size)`, which no pipeline could parse.
- **The semantics that were wrong are fixed, not merely present.** `cat` no
  longer stops at 512 bytes, `cp -r`/`rm -r` recurse through one shared `Walk`,
  `mkdir -p` creates parents, and `diff -u` → `patch` is a round trip that is a
  test case rather than a claim. `grep` and `test` exit 0/1/2, because a build
  driver reads a status.
- **`execve` resets the thread pointer.** `execve` installed the new image's
  `FS_BASE` only when the image carried a `PT_TLS`, so an image without one
  kept the old image's thread pointer and slibc's startup adopted it as an
  installed TCB: `echo x | tee f` faulted at `cr2=0x500000060`. The reset is
  unconditional now, as Linux has it. A builtins-only shell never saw it,
  because nothing before this put a fork-and-exec on a pipeline stage.
- **`canonicalize` resolves against the working directory.** The old std port
  joined a relative path onto `/`, so it answered the canonical path of a
  *different* file — successfully, whenever that file existed. `cp`'s
  copy-into-itself refusal is what found it.
- **The set is the POSIX floor a build needs**, 54 names from `ls` to `stty`.
  Three engines are written here rather than vendored, because nine
  third-party crates is the rule and a utility set is not a reason to change
  it: a POSIX regex engine (BRE and ERE, with a step budget, because the
  pattern is user input), DEFLATE plus gzip framing with the CRC verified on
  read, and SHA-256.
- **The privilege story is unchanged.** `/bin` and `/sbin` stay sealed and the
  grant table is keyed on the path `exec` resolves to, which for all 54 names
  is `/bin/coreutils` — an entry with no grant. The visible cost is that `ps`
  shows `coreutils` rather than the name that was typed.

**Five divergences and costs, stated rather than hidden.**

- **A non-UTF-8 operand is refused**, because every utility reads
  `std::env::args()` rather than `args_os()`. A byte-clean path is expressible
  on this target now; this is a change to 54 utilities, not to a target.
- **`ls -l` cannot report a mode, an owner or a link count.** `Metadata` here
  carries length, type, mtime and a read-only bit, so the permission string is
  derived from the type and the link count prints as 1. `test -r/-w/-x` answer
  from existence, which is the single-user decision below.
- **`sed` has no branching**, and `cp -p` preserves a file's mtime but not a
  directory's — there is no path-taking `utimensat` in the wrappers.
- **`gzip` encodes with fixed Huffman blocks**, falling back to stored, so its
  output reads everywhere but is larger than GNU gzip's. `inflate` handles all
  three block types.
- **The shell links the utilities it does not run.** `shell.elf` went 640 KB →
  1.37 MB, because `help` and completion walk the tool table and an entry holds
  its `run` pointer. The alternative is a second metadata-only table — the
  drift this design prevents — so the ~730 KB is paid deliberately. A
  `/bin`-scanning completion costs nothing and is the right fix once new
  binaries are a thing that happens in-guest.

---

## A script can loop, branch and substitute

The sixth thing this plan rests on: what a build system writes is a *script*,
and a shell that cannot loop, branch or substitute is not a workbench however
many utilities it can reach. `shell_script_test` is the standing proof — 33
cases against the real `/bin/shell` asserting on exact bytes, one of them on a
PTY because the continuation prompt exists only on the interactive path.
Between them: `if`/`while`/`until`/`for`/`case`, functions with their own
positional parameters, `break n`, `$(...)` and backticks, here-documents in all
four forms, globbing, the parameter-expansion operators, `$(( ))`, `"$@"`
against `$*`, a twelve-stage pipeline and a thousand-byte variable. Related
properties share one shell invocation deliberately: `MAX_PROCESSES` is 256, a
run reaches ~170 before this utest, and a spawn per assertion measured 243 held
at the phase boundary with the next dozen answering `ENOMEM`.

Everything pure about the grammar is `shell-core`, 77 host tests under `just
test-host`: tokens, the tree, the parser, POSIX pattern matching, IFS splitting,
the `${...}` operator split and arithmetic. What stayed in the application is
what has to talk to the kernel — variable lookup, command substitution,
pathname expansion's walk, and execution.

What it rests on, in case a later phase disturbs it:

- **`Incomplete` is a third answer, and it is the whole mechanism.** Wrong
  input and unfinished input are distinguished, so one reader serves a script
  file and a PS2 prompt alike. The failure mode is precise and a test found it:
  `for; do` answered `Incomplete` because "no word token here" and "no token at
  all" shared a branch, so the reader swallowed the rest of the script waiting
  for a command that could never arrive.
- **Quoting is recorded per byte, not in band.** `QBuf` carries a flag vector —
  quoted, and came-from-an-unquoted-expansion. A sentinel byte answers the same
  question and is what several C shells use, but it collides with the arbitrary
  bytes a filename may hold.
- **Field splitting only ever touches an expansion's output**, and `"$@"` puts
  a hard field boundary between parameters. The one case the bytes cannot
  answer is an empty result — `cmd $x` passes no argument, `cmd "$x"` passes
  one — so the splitter is told whether the word held a quoted byte at all.
- **A command substitution's output is data.** Forked, piped, read to EOF,
  trailing newlines stripped; the bytes are split and globbed but never
  re-tokenized, so a `;` among them is a byte the command receives. The inner
  text is lexed at expansion time, which is what makes nesting structural
  rather than a counter.
- **An unmatched pattern is left as written, and a generated pathname must
  exist.** No `nullglob`; a wildcard matches neither a leading `.` nor a `/`;
  a field with no unquoted metacharacter is not globbed at all, so `rm *.o`
  with no object files runs `rm` with a literal argument rather than none. A
  literal component *after* a wildcard is checked too. The matcher has a single
  backtrack point rather than a recursion per `*` — nine stars against forty
  bytes took eleven seconds the other way, and both a `case` subject and a
  `${x##pattern}` value are script-controlled.
- **A here-document's writer is a separate process**, or a body past the pipe's
  4 KiB capacity blocks the shell on its own read end. It is reaped *after* the
  descriptors are restored, or a command that read part of a long body
  deadlocks the reap.
- **Where a command runs is decided per command.** A builtin, a function and a
  compound command run in this shell, so `cd`, an assignment and a loop counter
  survive; an external program, a `( )` subshell and every pipeline stage run
  in a fork. Redirections follow: applied around an in-shell command and undone
  after, applied *in* the child otherwise, so an unopenable path is the child's
  status and the shell's descriptors are never at risk.
- **A redirected builtin has one output mechanism, not two.** The executor
  `dup2`s and restores; the global "write here instead" descriptor is gone,
  and with it the question of which was in force.
- **Only exported variables reach a child.** The table exported everything
  before, which is how a stray assignment changes what a configure script
  decides. It is heap-backed now too: 64 entries of 256 bytes silently
  truncated a `PATH` with a dozen entries in it.
- **`set -e` does not fire inside a condition.** A condition-depth counter is
  what keeps errexit off `if p; then`, `&&`, `||` and `!`. `break`, `continue`
  and `return` reach their construct through a requested control flow rather
  than a return value, because a builtin's signature is a status — which is
  also what makes `eval break` work with no second mechanism.
- **Two things must see past a name's first meaning.** `command NAME` resolves
  blind to the function table, or `ls() { command ls -F "$@"; }` recurses until
  the stack runs out; `unset NAME` names a *variable* first.
- **A redirection's backup is taken before its target is opened.** The kernel
  hands out the lowest free descriptor, so `3>out` in a shell holding 0/1/2
  opens exactly fd 3 — a backup taken afterwards captures the file itself, and
  the close that follows drops the only copy.

**What this deliberately did not do.** No `trap` — a disposition table the
shell consults at every exit path, which belongs with whichever phase first
needs a failed build to clean up after itself. No `local`, `getopts` or
aliases: none is POSIX-required and each is a scoping or parsing mechanism
rather than a widening. None of the non-POSIX conveniences (`$'...'`, brace
expansion, `[[ ]]`, arrays, `select`, `;&`), and `>|` behaves as `>` because
`set -C` does not exist to distinguish them. Arithmetic is signed 64-bit and
wraps. `time` is a builtin over pre-expanded words rather than the reserved
word POSIX makes it, so `time a | b` times `a`. `set NAME=VALUE` is kept beside
POSIX `set --`, because the shell accepted it before this.

---

## The terminal is one an editor can be written against

The seventh thing this plan rests on: a full-screen program can read the
keyboard, read the mouse, ask the terminal what it is, and draw a frame.
`terminal_grid_test` is the standing proof — it drives the real encoder and
emulator and checks exact bytes: F1 as `SS3 P`, Ctrl+Left as `CSI 1;5D`, Up as
`SS3 A` under DECCKM, Shift+Tab as `CSI Z`, a click as `CSI <0;10;5M`, a bare
move refused under button-event tracking and reported under any-event, `CSI c`
answered `CSI ?1;2c` and `CSI >c` answered with nothing printed, and `CSI 6n`
answered with the cursor's position. Every one was a dropped key, a wrong
answer, a stray glyph or a refusal before.

What it rests on, in case a later phase disturbs it:

- **A key is identified by its canonical keycode, not by a pseudo-byte.** The
  driver bakes a legacy `ascii` code for nine navigation keys and 0 for
  everything else, and `classify` then discarded the canonical HID keycode and
  the per-event modifier byte the compositor had carried the whole way — both
  drops, not absences. `encode_key` takes a `KeyPress` holding ascii, keycode,
  codepoint and mods; a baked navigation byte resolves first, because it is
  what survives a keypad key whose layout meaning is navigation, and
  everything the driver left anonymous resolves from the keycode.
- **The encoding is xterm's PC-style one, and the modifier is a parameter.**
  `1 + shift + 2*alt + 4*ctrl` in the second CSI parameter. F1–F4 are `SS3`
  unmodified and `CSI 1;mod P`–`S` modified; DECCKM is honoured only for an
  *unmodified* cursor key, because a modified one needs the parameter slot
  `SS3` does not have. AltGr is excluded from the Alt bit — the kernel reports
  it with `MODIFIER_ALT` set, and counting it would turn an AltGr-resolved `@`
  into a modified keypress.
- **Alt is a prefix, Shift+Tab is a sequence.** Alt+x is `ESC x`; Shift+Tab is
  `CSI Z`, and the modifier snapshot is the only thing that can produce it
  because the keymap folds both to 0x09.
- **PgUp/PgDn belong to the application.** Local scrollback moved to
  Ctrl+Shift+PgUp/PgDn, and the kernel's own Shift+PgUp interception had to
  learn to require Shift *without* Ctrl or the chord would never reach a
  client.
- **Mouse reporting is the application's, and Shift is the way out.** DECSET
  1000/1002/1003 are one selector as xterm has them, so resetting any stops
  reporting; 1006 selects SGR. Shift overrides to local selection, which is the
  only reason one stays possible under a full-screen program. Motion emits one
  report per *cell crossed*, and X10 refuses a coordinate past 223 rather than
  truncating it into the wrong cell.
- **A query is answered on the turn it was asked.** The reply queue drains
  immediately after `drain_master`, or a program blocked reading a CPR waits
  for a keystroke. It is 256 bytes and drops a whole answer rather than
  truncating one, and a reply must not cancel a deferred autowrap the way
  every other non-printing action does.
- **A CSI private marker is tracked rather than aborted on.** `>` used to drop
  the parser to Ground, so `CSI > c` printed a literal `c` the moment an editor
  probed for a secondary DA. Splitting dispatch by marker also closed the
  quieter half: the old dispatch consulted the marker for `h`/`l` only, so
  `CSI ? 5 m` turned a mode query into a blink attribute.
- **The shell's own decoder understands what the terminal now sends.** It
  answered `Partial` for anything parameterised until eight bytes, so one
  Ctrl+Left swallowed the next characters typed. It recognizes the whole
  CSI/SS3 shape now and consumes a well-formed sequence *whole*. `ESC` plus a
  byte that cannot begin a sequence is re-emitted rather than consumed, or
  Alt+x would type nothing and Alt+ä would lose its lead byte.
- **The glyph set is the blocks a TUI draws with** — 194 slots became 1190 over
  twelve ranges, which is what the shipped JetBrains Mono actually covers, so
  "non-Latin renders" means the scripts the font has.
- **Box drawing and block elements are drawn, not rasterized.** JetBrains
  Mono's box glyphs do not span the em box, and a centred-and-clipped glyph
  leaves a seam at every cell boundary — a framed TUI that looks broken.
  `font/src/boxdraw.rs` draws U+2500..U+259F procedurally as kitty and wezterm
  do: one `Geom` derives the midlines and thicknesses from the cell once, so a
  weight lands on identical rows in every glyph carrying it. The lines are a
  128-entry weight table plus one renderer; the eighths are
  `round(n * extent / 8)` so `█` equals `▀ | ▄` byte for byte; the shades are a
  4×4 Bayer dither so a region reads as texture at any cell size.
- **The atlas is chunked, and a missing glyph is the notdef.** 1190 slots at
  the ABI's 32×32 cell is 1.2 MB, past `MAX_ALLOC_SIZE`, so storage is
  `KVec<KVec<u8>>` in 256 KiB pieces and `SYS_FONT_SET` copies into each chunk
  rather than materialising the upload. A set codepoint the font lacks reads
  back the replacement diamond — without which growing the set by a thousand
  slots would turn a visible notdef into an invisible one.

**What this deliberately did not do.**

- **The kernel vconsole answers no query.** Its reply would have to reach the
  line discipline of the TTY whose write lock it runs under, so a DA or DSR on
  `/dev/tty0` is ignored rather than answered wrongly. Routing it through the
  deferred `PostLockWork` the echo flush already uses is the shape of the fix,
  and it is a TTY-layer change.
- **No `modifyOtherKeys`, no CSI-u, no Kitty keyboard protocol.** Ctrl folding
  happens in the kernel keymap and covers letters only, so `Ctrl+;` cannot be
  reported at all. That is a keymap gap with a terminal-visible symptom.
- **No focus reporting, no 1005/1015, no SGR-pixel.** Focus needs an event the
  compositor does not send a client; the others are encodings nothing modern
  asks for once 1006 exists.
- **Bold is a brighter colour and underline is invisible.** Attributes are
  flattened into the colours at print time. Fixing it means an attribute byte
  per cell — `Cell` is 12 bytes across a 100×240 grid plus two 1000-row rings —
  and a new `Cell::is_blank`, which the whole reflow trim rests on.
- **No astral-plane glyphs and no CJK.** The TTF parser reads cmap format 4
  only, and the shipped mono font has no CJK to cover even if it did.

---

## An editor you can work in

The eighth thing this plan rests on, and the last of the workbench: a file can
be opened, changed and written back without a Linux host in the path.
`/bin/editor` — Sloped — is a native GUI application over `appkit` and the
compositor: a file tree, tabs, a code surface with highlighting, find and
replace, a command palette, a file finder and undo. `editor_test` is the
standing proof — every `editor-core` case against the target's allocator, plus
the application's state machine driven through the messages its widgets emit.
It was written rather than ported because nothing upstream is reachable before
a C frontend exists; what that bought beyond the editor is a toolkit that can
express one.

What it rests on, in case a later phase disturbs it:

- **The logic is a crate, and the crate is host-testable.** `editor-core` holds
  the buffer, motions, edits with undo, search, the fuzzy matcher and the
  syntax lexer and touches no syscall — the split `terminal-core` and
  `shell-core` already draw, and why 91 cases run in milliseconds under `just
  test-host` and again in the guest.
- **A line vector, not a rope, and the trade is stated.** What this edits is
  source on a machine whose root is tens of megabytes: the cost that matters is
  per-keystroke work inside one line, O(line), and a line insert, a move of
  `line_count` pointers. `MAX_LINES` is a million and `EDITOR_MAX_FILE_BYTES`
  is 8 MiB. A rope buys a logarithm at a complexity the whole crate would have
  to be tested against.
- **Positions are characters, never bytes.** One conversion exists for where
  `String` needs an offset. Tabs are the one place a *display* column diverges,
  which is why the surface carries both and a click inside a tab resolves to
  the side the pointer is nearer.
- **Undo is a transaction, not a keystroke.** Typing coalesces while it stays a
  run of single characters advancing from the last; a motion, paste or save
  seals the group. Replacing a selection, splitting a brace pair, moving a
  line, commenting a block and an electric dedent are each *one* Ctrl+Z.
- **Highlighting is a line at a time, against the state the line above left.**
  A block comment *is* that state, so a viewport costs the lines above it once
  and the viewport each frame, never the file. The cache is behind a `RefCell`
  because drawing is a read that happens to memoize. The lexer resolves what a
  *character run* is and never what a name means, which is why a keyword list
  and a delimiter table are enough.
- **The toolkit grew what the editor needed, and every application got it.**
  `appkit` had one fixed-cell font, which is why every SlopOS application read
  as a terminal wearing a window; it now has proportional UI text beside the
  fixed-cell atlas for content whose columns must line up, plus a virtualized
  code surface and tree, tabs, a menu bar, a focus-explicit line input, a
  splitter, cards and a procedural icon set.
- **Transient interaction state belongs to the application.** The widget tree
  is rebuilt on every message, so anything a widget remembered between a press
  and the move after it was already gone. Drags, click runs and resizes live in
  the application and are *given* to the widget.
- **A widget answers a key only when the key is its own.** Nothing ever sent
  `FocusGained`/`FocusLost`, so every `focused` flag was permanently false and
  the two widgets that answered keys without consulting it answered *every*
  key: a button took Enter and Space from whatever was being typed into, and a
  list took the arrows — which for the command palette meant running each
  command the selection passed over.
- **A drag that leaves the thing it started on is still a drag.** Two layers
  say so. `StackWidget` tells a move *and* a release to children whose rect
  they missed, because a widget that latched on a press is the one waiting for
  them; under that, the compositor holds the `wl_pointer` implicit grab, so
  dragging past the window edge does not hand the release to whatever is
  underneath. Without either, a selection follows a pointer with no button
  held and the next keystroke replaces text nobody selected.
- **A space is a character.** The keymap reports Space as a *named* key so a
  focused button can be pressed with it; text-entering widgets translate it.
- **The clipboard is the compositor's** — an fd-based transfer both ways, with
  control bytes other than tab and newline dropped on the way in, because a
  clipboard is untrusted input.
- **What the file had, the file keeps.** Line endings, a missing final newline
  and the indent unit are detected and restored. What is histogrammed is the
  *step into* a block rather than the absolute indent, because every indent is
  a multiple of the unit and a four-space file with enough nesting shows as
  many eights as fours. A binary file is refused rather than opened as
  replacement characters a save would write back.
- **A save cannot destroy what it fails to replace.** `File::create` truncates
  before the first new byte lands, so a partial write would leave the user's
  file gone while the editor held the only copy. A save writes a sibling,
  `fsync`s and renames, which the journal makes one atomic metadata operation.
  It also asks before landing on a file that is not the one the tab read,
  compared by mtime and length recorded at open and re-taken at every save.

**What this deliberately did not do.**

- **No syntax tree, and no `syntect`.** TextMate grammars want a regex engine
  and tree-sitter wants a C toolchain that does not exist yet. The cost is
  precision: a capitalized identifier is typed as a type and a lowercase one
  before `(` as a call, because that is what a lexer can know.
- **No LSP, no multi-cursor, no split panes, no file watching.** A file changed
  underneath the editor is therefore not noticed *while it is open*, only at
  the moment a save would overwrite it — which is where the damage would be.
- **The terminal editor was not written.** The GUI one is what landed, because
  the toolkit gap it closed is what every other SlopOS application needed and a
  TUI editor would have closed none of it. The terminal's own gaps above are
  therefore still open.
- **No shift-click without keyboard focus.** The compositor sends no modifier
  state with a pointer event, as Wayland does not; `appkit` stamps a press with
  the keyboard's most recent snapshot, which is stale if the modifier was
  pressed before the window had focus.

---

## The target is a host's target

The ninth thing this plan rests on, and the one that took the *std platform
layer* out of the remaining work entirely: `x86_64-unknown-slopos` is a
**unix-family Rust target over a real C library**, and `std` is upstream `std`.
What a SlopOS binary runs on is `library/std/src/sys/pal/unix`; the 20-file,
3 963-line bespoke platform layer beside it is deleted, and so is the 729-line
script that used to sed it into the rustup sysroot in place. `libc_abi_test` is
the standing proof — fourteen in-guest cases holding slibc to the libc module's
own declarations, from a zeroed `pthread_mutex_t` usable without `_init` to the
sigset narrowing that turns a 128-byte userspace mask into the kernel's
8-byte one. It exists because under this design a wrong struct is a
*miscompile* rather than a compile error, which was the one new risk the
decision carried; two of its cases were falsified before being believed, by
breaking `si_addr` and the `SCM_RIGHTS` count in the kernel.

What it rests on, in case a later phase disturbs it:

- **`restricted_std` was a string allowlist, and it is retired.**
  `library/std/build.rs` compares `CARGO_CFG_TARGET_OS` against ~45 names and
  flips the whole crate to `unstable` on a miss, which had forced that
  attribute into **65** files. One line retired all of them.
- **The sysroot is owned, not mutated.** `-Zbuild-std` resolves std from the
  host sysroot's own source tree, so the old flow had to edit the rustup
  toolchain in place — a shared, unversioned, silently drifting input.
  `scripts/make_slopos_sysroot.sh` builds and registers an owned clone
  instead; `AGENTS.md` documents it and `scripts/check_toolchain_pin.sh` holds
  it to the pin. Builds are `cargo +slopos`.
- **The forks are diffs, and the diffs are the PRs.** 665 lines over 14 files
  of `library/`, and 1 550 over 6 files of a pinned `libc 0.2.189`. Nothing is
  vendored: the tarball comes from the cargo cache or crates.io against the
  checksum in `toolchain/PIN`, and each patch is verified against its pinned
  SHA before it touches a file. That is what makes "upstream it and the fork
  shrinks to nothing" an end state rather than a hope.
- **`libc/src/unix/slopos/mod.rs` is one file, and every layout in it is
  Linux's.** 1 461 lines — types, structs, constants, the `CMSG_*`/`FD_*`/`W*`
  helpers, and one `extern "C"` block declaring **227** entry points. SlopOS's
  ABI already *was* Linux's at the numbers and constants, so the module is not
  a translation layer; it is a statement of what slibc owes.
- **The std diff is cfg-sites, not a parallel PAL.** Three new files
  (`os/slopos/{mod,raw,fs}.rs`, derived from upstream's own `os/redox/`) plus a
  dozen-line `sys/random/slopos.rs`; everything else adds a
  `target_os = "slopos"` arm to a list that was already there. Two arms
  *subtract*: `current_exe` is `Unsupported`, because there is no procfs and a
  process cannot name its own image, and `backtrace`'s unix arm is declined,
  because a userland binary is `panic = abort` with `.eh_frame` discarded and
  the libunwind symbols are not there to link against.
- **Synchronisation rides pthreads, which costs the std fork nothing.** The
  `target_family = "unix"` arm already selects `pthread`, so `Mutex`,
  `Condvar`, `RwLock`, `Once` and parking are slibc's futex primitives with no
  cfg-site at all. The price is paid on the slibc side, which is the right
  side: those objects grew to their declared glibc sizes and must work
  zero-initialised, because std allocates them with `PTHREAD_*_INITIALIZER`
  semantics and only sometimes calls `_init`.
- **slibc is a C library now, not a Rust crate that exports C symbols.**
  `libc.a` comes from a `slibc/staticlib` wrapper and `crt0.o` from a
  `slibc/crt0` crate — the single gate that turns "can a C program be built
  here" from no into yes, pulled onto this path by the unix-family decision
  rather than waiting for a C frontend. It is a wrapper rather than a second
  crate type on slibc because cargo emits every declared type in one rustc
  invocation, and a `no_std` staticlib's `#[panic_handler]` would then be a
  duplicate lang item for all 211 rlib uses. The 42 headers under
  `slibc/include/` are *generated* from the same patch hunk the compiler
  reads, so a header cannot drift from the export it describes.
- **The `slopos_` thunk layer is gone.** 40 exports existed only so a patched
  PAL could declare them and now carry their real C names. Seven keep the
  prefix because they name calls no libc has.
- **Two ABI divergences are closed, by force rather than by choice.** std's
  unix PAL reads `siginfo_t.si_addr` and `msghdr`/`cmsghdr`, so `si_addr` moved
  to offset **16** — into the union arm Linux has — `msghdr` became Linux's
  56-byte form with a real `*mut iovec` the kernel walks, and `cmsghdr` 16
  bytes. `sendmsg`/`recvmsg` were private *because* those layouts were not
  Linux's, so they took 46 and 47 and the private range compacted.
- **Swapping the platform layer found three real bugs, and they were in the
  kernel and in slibc rather than in the port.** A patched PAL had been hiding
  each by taking a different path.
  - **The kernel could not write into a page that was merely promised.** OSTD's
    user-copy validates the leaf and takes no fault, so the first `read(2)`
    into a fresh `Vec` answered `EFAULT`; every kernel→user write in the tree
    had targeted eagerly-mapped memory. `mm::user_copy` resolves the range
    through the ordinary fault path first, and the ordering is load-bearing:
    the populate runs *before* the copy's `KArc<VmSpace>` is taken, because the
    demand path refuses to install a page while another reference is live.
  - **A task killed on a fault reported `exited(139)`, not
    `signalled(SIGSEGV)`.** The fatal-fault path records `UserFault`, a
    diagnostic distinction; std reads the POSIX status word, where those are
    different answers and a build driver acts on which it got.
  - **`realpath` resolved a relative symlink target against the wrong
    directory**, so `/bin/ls -> coreutils` canonicalised to `/coreutils`.
- **The gate this replaces, replaced.** `patch_std.sh`'s arm-order check
  existed because a `cfg_select!` arm after the `_` wildcard is dead code that
  still compiles — it shipped once as a `ud2` in `std::process::exit`. That
  failure mode went with the script; the two that replace it are the fork
  drifting from the pin and a `libc` struct disagreeing with `abi/`. A third
  was found by building it: `git apply` inside a work tree resolves paths
  against the *repository* root, so a sysroot under `third_party/` got every
  path ignored and exit 0. Discovery is ceilinged and every patch re-checked in
  reverse, because a gate that cannot see a no-op is not a gate.

**What this deliberately did not do.**

- **The triple is still a JSON spec, not a built-in.** Tier 3 buys the name and
  ships **no artifacts**, so `-Zbuild-std` stays mandatory either way until
  tier 2. What a built-in triple costs is a stage-2 cross toolchain (~100 GB of
  build directory by the dev guide's own figure) and what it buys is dropping
  three flags from one script, so the tier-3 diff is written when the PR is.
- **Upstreaming has not happened.** The two patches *are* the two PRs, in the
  order the tier policy asks for, and the cost is a maintainer name on record
  and an `MIT OR Apache-2.0` licence on the contributed files. Nothing
  contributed is GPL'd kernel or slibc code.
- **Being a *host* was gated on proc macros, not on the target spec, and the
  gate is now open.** `rustc_driver` is `crate-type = ["dylib"]` and
  `invalid_output_for_target` rejects that outright when `!dynamic_linking`, so
  a static rustc that expands proc macros does not exist. The spec says
  `dynamic-linking: true` and the loader behind it exists; what remains is
  Workstream 2.1's built-in triple.
- **The three layouts std's unix PAL never reads stay divergent**: the
  truncated `ucontext_t`, the `termios2`-shaped `struct termios`, and `NSIG`
  at 32. They are binary-compatibility work, which is an open decision below.

---

## The backend is measured, and it is still LLVM

The tenth thing this plan rests on, and the one that decided which toolchain
the phases below are written for. The spike this plan asked for before anything
else has been run, and its answer is a pair of gates rather than a paragraph:
`just check-toolchain-coverage` holds every codegen backend to the capabilities
`targets/x86_64-slos.json` depends on and every linker to the constructs
`link.ld` uses. LLVM answers seven of seven and `rust-lld` eighteen of
eighteen; cranelift three of seven and `wild` twelve of eighteen. Verdicts and
their reasoning live in `scripts/gates/{codegen,linker}/`.

**The gate fails in both directions**, which is the whole reason it is a gate:
a `lacks` that becomes a `has` fails the run exactly as a regression does.
"cranelift cannot build this kernel yet" is a claim with a shelf life, and
nothing else in the tree would notice it expiring.

What it rests on, in case a later phase disturbs it:

- **cranelift cannot compile a kernel that must not touch the vector file.**
  SSE2 sits *below* cranelift's lowest x86 feature toggle and a float is
  `RegClass::Float` unconditionally in its x64 ABI, so soft-float is a property
  of the backend rather than a flag nobody passed; cg_clif returns no target
  features at all for `Os::None`, and rustc refuses the ABI on that basis. The
  cost is not a gate failing: a syscall or fault entering from userland saves
  no vector state, so one such instruction clobbers the interrupted task's live
  registers.
- **Two of cranelift's four gaps are silent, and they are why this is a gate.**
  `-Zemit-stack-sizes` is accepted and emits nothing, so S-5's 2 KiB ceiling
  against a 4 KiB guard page would be enforced by nothing; `-Zsanitizer=safestack`
  is accepted and instruments nothing, and its companion `-Cllvm-args` option
  does not even have a spelling. `check_stack_sizes.sh` fails closed on the
  first through `min-records`. The `sym`-operand gap is the one that is only a
  switch — the implementation sits under a `cfg!` the shipped component leaves
  off — and four sites here need it.
- **`wild` puts the output sections in its own order, which is the finding that
  disqualifies it.** That is a design property, not a default: its own suite
  skips lld comparisons under a group named `design_differences`, and
  `link.ld`'s order is load-bearing. Three more: it reads `. = X` as "the image
  starts at X" where ld and lld read "the next output section starts at X", so
  the first *section* lands past the base by an amount that depends on the phdr
  count; it refuses `. = <symbol>`, which is `link.ld` line 19; and it defaults
  `--gc-sections` on, where `KEEP` saves the eleven registries but nothing
  relying on ld's default would survive. Its refusals read "not in a release" —
  upstream has implemented the location-counter case and there is no release
  carrying it — which is the opposite of how soft-float reads, and the
  distinction the gate exists to keep visible.
- **The "no floating point anyway" escape is false.** `libm` is in the kernel's
  dependency tree through `slopos-font`, and on this exact target spec it
  compiles to zero XMM instructions under LLVM and 19 037 under cranelift.
- **The spike found one real bug, and it was ours.**
  `targets/x86_64-slos.json` spelled its `llvm-target` `x86_64-unknown-none`
  where upstream's own spec spells it `x86_64-unknown-none-elf`; without the
  object-format component `target-lexicon` answers `BinaryFormat::Unknown` and
  cg_clif ICEs before compiling a line. A no-op for LLVM's codegen, which is
  why `object-format` is a probe rather than an assumption.

**What this deliberately did not do.**

- **Two spec properties are stated as residual rather than probed.**
  `disable-redzone` needs an optimised build and a disassembly heuristic to
  tell a red-zone spill from an ordinary one, and `panic-strategy: unwind`
  cannot be expressed in a standalone `no_std` probe at all. `link.ld`'s own
  `ASSERT` on `.eh_frame_hdr` holds the second at every kernel link, which is
  stronger than a probe.
- **No patch to cranelift, and no linker written here.** Soft-float in the x64
  backend is a lowering pass and a linker that honours this script is what
  `wild` has left; both are upstream-shaped work this plan does not pay for.
  What this workstream owed was the answer, and the answer is re-taken on every
  CI run instead of believed.
- **Userland is not gated, though it was measured.** cg_clif refuses slibc on
  variadics and hits the same `sym` wall in `slopos-ostd`, which every userland
  binary links; `wild` fails `userland.ld` on the same location-counter symbol
  and, with the base inlined, mislays the image so `USER_CODE_BASE` would stop
  matching `PROCESS_CODE_START_VA`. "Let the in-guest loop own userland and
  keep the kernel cross-built" needed a Rust linker for userland too, and does
  not have one either.
- **`mold` cannot read `link.ld` at all.** Its script parser understands five
  directives, none of them the ones here. Adding it is a `--linker mold` arm
  and a gate file when that changes.

**What it decided.** LLVM and `rust-lld`, cross-built from Linux, with the C++
runtime ported to SlopOS. That is not the answer this plan was written to want
— it is the one the measurement leaves. Writing SlopOS's sources around what a
backend cannot express would also make every third-party crate a compatibility
question, which is the opposite of what a development machine is for. Phases 1
and 2 are the consequence, and both gates keep their value as the thing that
would notice the day either candidate stops refusing.

---

## A program can be linked at run time

The eleventh thing this plan rests on, and the one Phase 2 could not have begun
without: a `PT_INTERP` executable runs on SlopOS, `dlopen`s a shared object and
calls into it. `dl_test` is the standing proof — it spawns `/bin/dl_probe`, the
tree's only dynamically linked program, which works through twenty-eight
ordered checks and exits with the number of the first failure: that it started
at all (so `PT_INTERP`, `AT_BASE`, the interpreter's own relocations, the
executable's `DT_NEEDED` and the static TLS block all worked), `dlerror`
reporting and clearing, `dlsym` on a function and on data, a call *out* of the
loaded object into a symbol the executable exports, a thread-local inside it
through `__tls_get_addr`, `dladdr` and `dl_iterate_phdr`, and `dlclose`
unmapping it. A second case writes into the object's RELRO region and is graded
on dying by `SIGSEGV` — *which* signal, because a probe that failed to start
also dies without an exit code.

**`libc.so` is the interpreter, and that is the whole design.** One artifact,
193 064 bytes and 892 exported symbols, entered at `_dlstart` when the kernel
runs it as an interpreter and linked as `-lc` when a program needs the C
library; `/lib/ld-slopos.so.1` is a symlink to it. musl ships exactly this
shape, and it is what makes the two-libc bug Redox names — "an allocator
mismatch between the libc and the dynamic linker" — *unexpressible* rather than
fixed: one allocator, one `errno`, one TLS implementation and one object table
in a process however many objects it loads. The loader is 2 406 lines under
`slibc/src/ld_so/`, the same crate as everything it has to agree with.

What it rests on, in case a later phase disturbs it:

- **The kernel loads two images, and the second one's address comes from the
  gap finder.** `process_vm_map_interpreter` places the interpreter's `ET_DYN`
  segments at a `find_gap` base with one VMA per segment carrying that
  segment's own protection. The base is not a constant: the VMAs are what stop
  a later `mmap` landing on the interpreter, and a constant would have needed
  a new band in `memory_layout_defs.rs`'s ascending chain.
  `UnsupportedLoadBase` survives, because an `ET_EXEC` image away from
  `PROCESS_CODE_START_VA` still cannot be shifted.
- **The executable's bytes are streamed after the interpreter is mapped, and
  the ordering is load-bearing.** Mapping takes the page-table cursor, which
  refuses to install a leaf while a second reference to the address space is
  live, and streaming holds exactly such a reference — the first version held
  the `KArc<VmSpace>` across the interpreter's map and got `WouldBlock` out of
  every map call. It is the same rule the user-copy populate path learned from
  the other side.
- **`AT_BASE` is always emitted, even at zero**, so the vector is a fixed seven
  pairs and `EXEC_ARG_STACK_FIXED` and the stack's alignment slot count can be
  stated against it rather than recomputed per exec. Linux does the same.
- **A `PT_LOAD`'s leaf permissions come from its own `p_flags`**, which they
  did not before: the eager mapper picked raw `USER_RW`/`USER_RO`, neither
  carrying `NO_EXECUTE`, so every mapped page of every process was executable —
  harmless while nothing described a page otherwise, and a false statement the
  moment the interpreter's per-segment VMAs say `exec: false`. A page two
  segments share takes the *union* of what each asks for — the first page of a
  Rust binary holds the program headers and the first bytes of `.text`, so
  keeping only the earlier segment's permissions hangs the boot — and that
  union is resolved before the first map rather than by re-protecting
  afterwards, because a `protect` on a live leaf issues a TLB shootdown and
  waits for every peer to acknowledge it, which is cross-CPU work inside
  `exec`'s mapping loop for an address space no CPU is running yet. The eager
  anonymous `mmap`, ring and COW paths still use the raw constants;
  `CVSS.md`'s SLOPOS-2026-0056 owns them.
- **The bootstrap touches no pointer that lives in memory.** `_dlstart` takes
  its load base from `lea rip + __ehdr_start` — the ELF header sits at vaddr 0
  in a shared object, so its RIP-relative address *is* the bias — walks its own
  `PT_DYNAMIC`, and applies `R_X86_64_RELATIVE` and `DT_RELR` and nothing else.
  There are 229 of them in `libc.so` and exactly one `JUMP_SLOT`, which is
  `main`. Everything past that point is ordinary Rust.
- **`-Bsymbolic` is what keeps that bootstrap honest.** Without it the
  interpreter's own references are preemptible `GLOB_DAT` relocations, and
  resolving one needs a string table pointer the pass has not fixed yet.
- **Binding is eager, and full RELRO is what that buys.** `.rela.plt` is
  processed exactly like `.rela.dyn`, so there is no PLT trampoline, no
  `GOT[1]`/`GOT[2]` handshake and no resolver running with a caller's argument
  registers live — the part of a lazy loader that is all ABI and no algorithm.
  Nothing writes a GOT slot after relocation, so `PT_GNU_RELRO` is sealed whole
  rather than stopping short of `.got.plt`, and the `SIGSEGV` case proves it.
- **TLS is one table, and the loader fills it.** A static program registers its
  own module from `AT_PHDR` as before; under an interpreter the loader
  registers every startup module and assigns each an offset below the thread
  pointer. A thread's block is that layout plus a TCB plus a DTV, and a
  `dlopen`ed module's slot is allocated on first access. A `TPOFF64` against
  such a module is refused (`RelocError::DynamicStaticTls`) rather than
  resolved to a wrong offset, which is what musl does.
- **A handle is the address of the loader's table entry**, so it cannot be
  forged from an integer and a closed slot's address fails the liveness check
  rather than naming whatever took its place. `dlclose` refuses an object
  `dlopen` did not load, because `dlopen(NULL)` hands back the executable's
  entry.
- **`compiler-builtins-mem` is off for the shared library, and that is a
  finding rather than a tweak.** It defines `memcpy`, `memset`, `memcmp` and
  `strlen` with *hidden* visibility, which wins over slibc's own and leaves
  them out of `.dynsym` — measured, so a C program linking `libc.so` could not
  call the four functions a C program calls most. Nothing static notices,
  because an archive has no dynamic symbol table to be absent from.
- **The link line moved out of the target spec.** `pre-link-args` applied
  `-Tuserland/userland.ld --emit-relocs` to every artifact, and a shared object
  must have neither: `userland.ld` fixes an image at 0x400000 and discards
  `.interp`. Static binaries take those from `scripts/build_userland.sh`; the
  shared objects take none, and `--image-base=0x400000` is what produces a
  correct non-PIE dynamic executable.

**What this deliberately did not do.**

- **Four of the six target-spec fields did not flip.** `dynamic-linking` is
  `true` (the `cdylib` crate type needs it) and `tls-model` is
  `global-dynamic` (a ceiling — LLVM still lowers a non-PIC executable's own
  thread-locals to local-exec, which is why the static binaries are
  byte-for-byte unaffected). `relocation-model` and PIE are met per invocation
  instead, and the interpreter does not collide with the executable because it
  is placed in the mmap arena. `panic-strategy` and `eh-frame-header` were the
  unwinder's to decide; `eh-frame-header` has since flipped to `true` with the
  C++ runtime, which is the section below.
- **The system's own binaries stay static.** Every one links slibc as an rlib —
  211 `slopos_slibc::` uses — so making them dynamic is a userland refactor,
  and it would put `/sbin/init` behind the loader for no behaviour it does not
  already have. The cost is that the loader is exercised by one program rather
  than by the whole boot, which is what `dl_test` is for.
- **No lazy binding.** The cost is that a program pays for every PLT slot at
  startup: at 100 000 slots that is a GNU hash lookup each, which is the scale
  `librustc_driver.so` will ask for and the number to re-measure when it does.
- **`R_X86_64_COPY` is implemented and unexercised.** It arises only for a
  non-PIE executable referencing a shared library's *data*, the toolchain
  binaries are PIE, and covering it needs a third fixture object — the
  `dlopen`ed one cannot serve, because making it a `DT_NEEDED` is what would
  stop the unmap case testing an unmap.
- **No symbol versioning, no `DT_RPATH`/`DT_RUNPATH`, no `LD_LIBRARY_PATH`, no
  `LD_PRELOAD`.** The search path is `/lib` then `/usr/lib`, and a name with a
  slash is used as written. `DT_VERSYM` is not consulted.
- **No `_r_debug`, no `link_map` chain, `DT_DEBUG` left at zero.** A debugger
  attached to a dynamic SlopOS program sees the executable and nothing the
  loader mapped. It is a dozen lines and a protocol, and it buys debugging
  rather than running.
- **`dlerror` is process-wide.** POSIX allows it; glibc and Redox make it
  per-thread. Doing the same means a thread-local in the loader, and the loader
  deliberately touches none.
- **The object table is 128 entries and never grows.** A `dlopen` past it is
  `ENOMEM`, because the table is what `dl_iterate_phdr` walks and what a handle
  is the address of, so its entries have to keep their addresses.

---

## A C++ exception crosses an object boundary

The twelfth thing this plan rests on, and the one Phase 2 cannot be attempted
without: a cross-built C++ program runs on SlopOS against a C++ standard
library, and an exception thrown inside a `dlopen`ed object is caught by type
in the executable that loaded it. `cxx_test` is the standing proof — it spawns
`/bin/cxx_probe`, one of the tree's two cross-built C++ programs, which works
through twelve ordered checks and exits with the number of the first failure:
a local throw and catch, `dlopen`, the loaded object's `std::vector`, its static
constructor, its `std::string`, the throw across the boundary caught as
`CxxTestError&`, a destructor in the unwound frame proving phase-2 cleanup
ran, a foreign type reaching `catch (...)` and not the typed handler, an
exception thrown past a live `unique_ptr`, `std::exception_ptr` and
`std::rethrow_exception`, the loaded object's static *destructor* running on
`dlclose`, and a throw *from* one of those destructors, which unwinds inside an
object the loader has already marked dying. A second case throws with no handler anywhere and is graded on
dying by `SIGABRT` — *which* signal, because a probe that failed to start also
dies without an exit code.

**Nobody else has this test.** `unwinding`'s C++ claim is a README usage
instruction with no C++ exception test in the crate; Redox's only C++ test
contains no `throw`, and its libc++ recipe's own statement of testing is
"tested as far as compiling recipes".

**It is `libc++`, and the deciding fact was the second compiler.** The
open-decisions list carried `libstdc++` against `libc++` and said to settle it
by cross-building one. What settled it is that `libstdc++` is not a library you
cross-build, it is a library GCC builds: it arrives out of a three-stage
bootstrap of a GCC cross-compiler, which is a second toolchain to pin, patch
and keep. `libc++` is built by the clang that has to be present anyway, because
a C++ program for this target cannot be compiled without one. Redox's evidence
pointed the other way and was about Redox: relibc has no `link.h` and no
`dl_iterate_phdr`, so `LIBCXXABI_USE_LLVM_UNWINDER` was closed to it and
libgcc's `__register_frame_info` registry was the road left. Neither constraint
applies here.

What it rests on, in case a later phase disturbs it:

- **The runtime is one shared object, and that is not a packaging choice.**
  `scripts/make_slopos_cxx.sh` builds libc++ and libc++abi as static archives
  and links both, whole-archived, into a single `libc++.so` — 776 056 bytes.
  libc++abi keeps the caught-exception stack and the `type_info` a `catch`
  matches on in process-wide state, so two instances of it in one process is a
  throw that cannot be caught across the boundary between them. The link is
  written out in the script rather than left to CMake because clang has no
  toolchain for an unknown OS and hands the link to `gcc`, which would supply
  the host's crt objects and the host's libc. It is *not* `-Bsymbolic`:
  `operator new` is replaceable by the program, and binding libc++'s own calls
  to it locally is exactly what would stop that working.
- **`libc.so` exports the seventeen `_Unwind_*` entry points**, from
  `vendor/unwinding`. The C++ library's `__gxx_personality_v0` is the Level-2
  half; putting the Level-1 half anywhere else would be a second object to find
  before an exception can leave the frame that threw it, in a process that
  already has exactly one artifact it must agree with about allocation, `errno`
  and TLS. `libunwind` is not ported.
- **Finding an FDE is `fde-custom`, and the two roads this plan named are both
  closed.** `fde-phdr-dl` reaches `dl_iterate_phdr` through the `libc` *crate*,
  which slibc cannot depend on, being what that crate declares; `fde-registry`
  needs a `crtbegin` calling `__register_frame_info` per object, which this
  target has not got. So slibc registers a finder of its own that answers out
  of the loader's object table — the same walk `dl_iterate_phdr` does, with one
  fewer indirection and no dependency at all — under the loader's lock, so a
  concurrent `dlclose` cannot free the headers underneath it. A static program
  has no loader table and its one object's headers come from `AT_PHDR`.
- **The artifacts that carry the unwinder are built `-C force-unwind-tables`,
  and this was the whole bug.** `_Unwind_RaiseException` saves its own context
  and looks the resulting return address up first, so the first frame of every
  unwind is one of its own — and a `panic = abort` Rust artifact emits no
  `.eh_frame` at all. Without the flag, every throw ended at frame zero with
  `_URC_END_OF_STACK`, which `__cxa_throw` turns into `std::terminate` printing
  the exception's type: a program that looks exactly like one with no handler.
  `eh-frame-header: true` in the target spec is the other half, so every shared
  object carries the `PT_GNU_EH_FRAME` the finder looks for. The static
  Rust binaries are unaffected either way: `userland/userland.ld` discards
  `.eh_frame` outright, and they are `panic = abort` Rust that never unwinds.
  `cxx_static_probe` is the case that needs the tables and it does not use that
  script — it links `libc.a` and `libc++.a` directly, with `--eh-frame-hdr` and
  `--image-base=0x400000`.
- **The libc gap was sixty-eight symbols wide and is now zero.**
  `scripts/check_cxx_pin.sh` holds it there: every symbol `libc++.so` leaves
  undefined must be one `libc.so` exports. The C++ library is linked without
  `-z defs` — an undefined symbol in a shared object is legal and the loader
  resolves it at load time — so without this gate a libc gap is not a link
  error but a `dlopen` that fails on a machine, at the point the runtime is
  first needed.
- **The headers became C++ headers, and the generator now enforces it.** Every
  generated header wraps its declarations in `extern "C"`; a typedef whose name
  C++ reserves (`wchar_t`) is emitted for C only; and a parameter named with a
  C++ keyword is a generation error rather than a silent rename, because the
  fix belongs at the declaration the header is generated from. `rename`'s and
  `renameat`'s `new` parameters were the two that had to move.
- **What the C++ runtime actually needed from the C library was measured, not
  guessed.** `<math.h>` (complete for `double` and `float` over the vendored
  `libm`, with classification and comparison as compiler builtins so they cost
  no symbol), `<ctype.h>`, `<assert.h>`, `<uchar.h>` for `mbstate_t`,
  `remove`, the `abs` and `div` families, `aligned_alloc`,
  `strtod`/`strtof`/`strtold`/`strtoll`/`strtoull`, `pthread_once`, the POSIX
  option macros in `<unistd.h>` (`_POSIX_TIMERS` is what `steady_clock` tests
  for, and without it libc++ stops with `#error`), and `<sys/time.h>` pulling
  `<time.h>` so the `CLOCK_*` ids are visible where a portable program looks
  for them.
- **`atexit` and `__cxa_atexit` are one list.** C++ requires the two orders to
  interleave, so a destructor registered after an `atexit` handler runs before
  it; two lists cannot express that. Order is a per-registration sequence
  number rather than a slot position, so a slot a `dlclose` frees is refilled
  by the next registration — otherwise a `dlopen`/`dlclose` loop consumes the
  table monotonically — while `__cxa_finalize` still takes the highest
  sequence and not the highest index. The lock is not held across a
  call, because a destructor may register another one, call `exit`, or unload
  the object it belongs to. Redox's `cxa.rs` does the opposite of all three,
  and its `exit` never calls `__cxa_finalize` at all.
- **A static program's `.init_array` had no one to run it.** A dynamically
  linked program's constructors are the loader's `DT_INIT_ARRAY`, and until
  `cxx_static_probe` existed nothing linked statically had any: a `panic =
  abort` Rust binary emits an empty array, and lld then defines
  `__init_array_start` and `__init_array_end` at the same address. A static C++
  program cannot start that way — every namespace-scope object's constructor is
  in there — so `__slibc_start` walks the bracketed range itself when the
  loader's table is empty, which is the one condition under which nobody else
  will have.
- **`dlclose` finalises by span, not by handle.** `__dso_handle` is hidden and
  absent from an object's `.dynsym`, so there is nothing for the loader to look
  up; every handle a C++ object registers with points into its own image, which
  makes the mapped span the question that can actually be asked.
- **`R_X86_64_COPY` is exercised now.** A non-PIE C++ executable against a
  shared C++ library produces five of them in `cxx_probe` — `stderr`,
  `std::exception`'s `type_info`, and vtables — which is the relocation the
  loader implemented and had no program to prove.
- **A utest can say why it failed.** A userland test's stdio is init's console
  rather than the serial line a run is read from, so a case that fails silently
  is a name and nothing else. `test_harness::note` attaches a message to the
  KTAP line, which is the only channel that reaches a reader; a case that
  drives a second program has nothing else to report its exit status through.

**What this deliberately did not do.**

- **The runtime is built with localization, wide characters, `<filesystem>`,
  the random device and the time-zone database off.** Those are the parts of
  libc++ that need a locale layer SlopOS has not got — the `*_l` family,
  `wcstof`, `wcstold`, `nl_langinfo` — and turning them off compiles them out
  rather than stubbing them. `<iostream>`, `<regex>`, `<locale>` and `<fstream>`
  are therefore unusable. Redox's libc++ is configured the same way and for the
  same reason.
- **`long double` is `strtold` and nothing else.** It is x87 80-bit on this
  target, Rust has no type for it, and the System V ABI returns it in `st(0)`
  — which no Rust signature can name — so the one entry point libc++ needs to
  build is two instructions of assembly that widen `strtod`'s `double`. It
  carries `double` precision, stated. The `long double` math family is absent;
  a C++ `<cmath>` lowers those overloads to `__builtin_*l`, which only becomes
  a call to a missing symbol if a program uses one.
- **`strtod` does not read hexadecimal significands.** The scan stops at the
  `x`, which is C89's reading of it and a refusal a caller can see in `endptr`
  rather than a wrong value.
- **The C++ runtime reaches the tests image only — the unwinder does not.**
  `libc++.so` as a file and `libc++.a` inside `cxx_static_probe`; the shipped
  appliance root runs no C++ program, and a megabyte of runtime nothing links
  belongs on the dev disk Phase 2 builds rather than in the image `just boot`
  attests. `libc.so` is a different matter: the `unwinder`
  feature is on for it unconditionally, so every image's C library carries
  `unwinding` and the DWARF reader behind it, and every process registers the
  frame finder at startup. That is what `--gc-sections` cannot drop while
  startup names the finder, and building a second, feature-off `libc.so` for
  the shipped image would mean two C libraries that differ between images —
  a worse hazard than the bytes. Measured: 412 384 against 241 376 before.
- **The C++ programs are the two probes and no more.** Nothing in the system
  is written in C++ and nothing should be; the runtime exists so that a
  cross-built LLVM can run, which is Phase 2's first measurement.
  `cxx_static_probe` is not a second feature — it is what keeps `libc++.a`,
  `libc.a`'s unwind tables and the finder's `AT_PHDR` road from being three
  things nothing has run.

---

## Phase 1 — The libc surface

**Outcome:** the C library has what a cross-built LLVM asks of it.

This phase did not exist while the toolchain was going to be Rust-hosted; it
is what the LLVM decision buys. Everything in it is userland, so none of it
touches the framekernel discipline. It opened with a dynamic loader and a C++
runtime, both of which have landed — "A program can be linked at run time" and
"A C++ exception crosses an object boundary" above — and what remains is
**M**: the rest of the libc surface underneath them.

### Workstream 1.1 — The libc surface (**M**)

This is the surviving half of what used to be "A C toolchain, written in
Rust", and the LLVM decision promotes it from off-the-critical-path to
load-bearing. What a C++ standard library needs was settled by building one:
`<math.h>` for `double` and `float`, `<ctype.h>`, `<assert.h>`, `<uchar.h>`,
`remove`, the `abs` and `div` families, `aligned_alloc`, the `strto*` family,
`pthread_once` and the Itanium ABI's `__cxa_atexit` trio all landed with it,
and `scripts/check_cxx_pin.sh` holds `libc++.so` to exporting nothing the C
library has not got. What an in-guest rustc adds beyond that list:

- **`<setjmp.h>`:** `setjmp`/`longjmp`, absent. Eight callee-saved slots and
  the return address, and the layout is re-derivable from the System V AMD64
  ABI rather than copied.
- **`<locale.h>`'s functions:** `setlocale`, `localeconv`, `nl_langinfo`. The
  header exists and declares `struct lconv`; nothing implements it. libc++ is
  built `LIBCXX_ENABLE_LOCALIZATION=OFF` precisely because this is missing, so
  this workstream is what would make `<iostream>`, `<regex>` and `<fstream>`
  compile at all — and nothing in the toolchain needs them, so it is the
  lowest item here.
- **`<wchar.h>`, which does not exist**, and with it `wcslen`, `mbrtowc`,
  `wcrtomb`, `wcstof`, `wcstold`. `mbstate_t` landed in `<uchar.h>` and is
  laid out so a later `<wchar.h>` can define the conversions against it
  without changing it. Redox's in-tree TODOs name exactly `wcstof` and
  `wcstold` as what keeps its libc++ narrow too.
- **The `long double` family**, which `strtold` is the single stated exception
  to: x87 80-bit arithmetic that neither Rust nor the vendored `libm` can
  express. Needed only by a program that writes `long double`, which rustc's
  LLVM does not.
- **Hexadecimal significands in `strtod`**, which C99 requires and the current
  scan refuses at the `x`.
- **The old list, minus what has since landed:** `qsort`, `bsearch`,
  `strerror` (`strerror_r` exists, the plain form does not), and the
  `<time.h>` calendar — `localtime`, `mktime`, `gmtime` and `strftime` are all
  absent where `gettimeofday` and `clock_gettime` are not.

**What this workstream stops owing.** The C99 frontend written in Rust
emitting cranelift IR is **deleted**, not deferred. Under the Rust-hosted road
it was the only way a C program could ever be compiled here, and `saltwater`
was its reference design. Under this road clang is cross-built in the same
monorepo pass that produces `libLLVM.so` and `rust-lld`, so the C compiler
arrives as a by-product of a decision taken for Rust's sake. That is a real
scope reduction — one **M**/**L** workstream removed — and it is the only place
the LLVM road is cheaper than the road it replaced.

**Phase 1 exit criteria:** `libLLVM.so`, cross-built and linked against
SlopOS's `libc++.so`, resolves every symbol it needs from `libc.so`.

---

## Phase 2 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

This phase is **XL**, and the decision it opened with has been measured and
then reversed. It read: **the Rust toolchain is Rust-hosted** — rustc with the
cranelift backend and a Rust linker, no LLVM — on the grounds that declining
LLVM declines a **C++** toolchain port, which is the expensive part, and says
nothing about C. "The backend is measured, and it is still LLVM" above is the
spike that tested it, and it came back negative on both halves against this
kernel. The decision is now **LLVM, cross-built, with the C++ runtime ported**
— Redox's road, and the only one anybody has walked. The section above carries
why that is forced rather than chosen; the short form is that soft-float is a
property of cranelift's x64 backend and not a flag, and that the escape which
looked available on our own side of the fence was measured false.

**What survives the reversal.** `.stack_sizes` and safestack were the two gaps
this plan could have afforded to lose — neither is portable Rust, and
Rust-for-Linux has no Rust frame-size check at all — but they cost nothing now,
because LLVM answers seven of seven. `rust-lld` answers eighteen of eighteen
and arrives in the same monorepo build as `libLLVM.so`, so **`wild` leaves the
critical path entirely**: the linker question is answered by the thing already
being paid for. `scripts/check_linker_script.sh` keeps its whole value as the
ratchet that would notice `wild` becoming viable, which is now a reason to
re-open a decision rather than a blocker to route around.

**What the reversal costs.** Phase 1 above, in full. The dynamic loader was
always owed; the C++ runtime was new, and both have since landed. What is left
of that cost is the libc surface, which was owed either way.

### Workstream 2.1 — The target becomes a host (**M**)

`x86_64-unknown-slopos` is a JSON target today, and that is enough to *build
for*. It is not enough to *build rustc for*: bootstrap's `--host` resolves a
triple through the compiler's own built-in list, and `rustc_driver` is
`crate-type = ["dylib"]` in `compiler/rustc_driver/Cargo.toml` — hard-coded, so
there is no configuration in which a host rustc is a static binary. The target
therefore has to become a built-in spec in `rustc_target/src/spec/targets/`,
which is what `x86_64-unknown-redox` is and what the fork already has the
machinery for: `toolchain/rust/` is a patch
series over the pinned channel, `toolchain/PIN` is where its hash is written
down, and `scripts/check_toolchain_pin.sh` already fails when a materialized
sysroot drifts from it. One more patch hunk, held by the same gate.

The Decided block below has said all along that the built-in triple is not
done and that tier 3 ships no artifacts, so `-Zbuild-std` stays mandatory
regardless. What changes is that it stops being a convenience and becomes a
prerequisite for bootstrap taking `--host=x86_64-unknown-slopos` at all.
Upstreaming it as a tier-3 target is worth doing for the maintenance it saves,
and is not on the critical path.

### Workstream 2.2 — The toolchain is cross-built and lands on a dev disk (**L**)

One bootstrap invocation on Linux, `--build=x86_64-unknown-linux-gnu
--host=x86_64-unknown-slopos`, producing rustc, cargo, `rust-lld`, clang and
`libLLVM.so` for SlopOS, plus the std built through the existing fork. Nothing
in that sentence is novel — it is how every cross-hosted Rust distribution is
produced — and everything in it depends on Phase 1, because every artifact in
it is dynamically linked.

**The medium is already reachable.** Measured from the host: the pinned sysroot
is 1.1 GB, `librustc_driver.so` is a single 161 MB shared object, and the
`libLLVM.so` beside it is 208 MB. Against that, `just test-capacity`
already builds a 16 GiB ext2 volume populated with this repository and that
sysroot, `mount(2)` takes a named device, and the verity hash array is chunked
so an image's ceiling is what RAM allows rather than what one allocation
allows. The toolchain disk is a second volume the installer reads, not a
redesign.

The better number is the one a self-hosted machine actually needs, and Redox
publishes it by shipping it. Measured off `static.redox-os.org`: its
`rust-install` is `rust.pkgar` (85 MiB) plus `llvm21.pkgar` (24.5 MiB) ≈ **110
MiB**, and a full native C/C++/Rust set — rust, llvm21, its runtime, clang21,
lld21, llvm-rt21, gcc13, gcc13.cxx, libstdcxx, libgcc — is ≈ **246 MiB** of
package payload. Its own automated self-hosted build config provisions a
**10 GiB** filesystem (`config/sys-build.toml`, `filesystem_size = 10000`)
against 650 MiB for its desktop image. Two orders of magnitude under the 1.1 GB
host sysroot, because a shipped toolchain is not a rustup toolchain.

**The C++ runtime this rests on is configured smaller than LLVM's own build
assumes, and that is the risk to re-take here.** `make_slopos_cxx.sh` builds
libc++ with localization, wide characters, `<filesystem>`, the random device
and the time-zone database off, which is the same configuration Redox uses and
is enough for `cxx_probe`. It is not known to be enough for LLVM: nobody has
published an LLVM built against a libc++ configured that way, LLVM's own
`LLVM_ENABLE_EH` and `LLVM_ENABLE_RTTI` default **OFF** while libc++abi's
exception machinery does not, and Redox's own recipe passes
`-DLLVM_ENABLE_RTTI=On`. The measurement Workstream 2.2 should take first is
therefore not "does the bootstrap finish" but "does `libLLVM` configure and
link against this libc++ at all" — a `<filesystem>` or `wchar_t` use in LLVM's
support library is a link error the first time, not a subtle one, and the
answer is either a narrower LLVM configuration or a wider libc++ one. Turning
localization back on is the wider one, and it costs the `*_l` family,
`nl_langinfo` and a locale layer slibc has not got.

**What is genuinely open** is whether `libLLVM` is shared or static.
Bootstrap's own default is **static** — `llvm_link_shared` is
`config.llvm_link_shared.unwrap_or(false)` and `LLVM_LINK_LLVM_DYLIB=ON` is set
only when that is true — and Redox opts *into* shared with `[llvm] link-shared
= true`. Static removes one `dlopen` from the critical path but not the
proc-macro one, and `rustc_driver` is `crate-type = ["dylib"]` in the
compiler's own `Cargo.toml` regardless. Decide it with the first bootstrap run
rather than on paper; the open-decisions list carries it.

### Workstream 2.3 — The build loop holds (**M**)

A toolchain that starts is not a toolchain that finishes. What the loop needs
beyond Phase 1, with the tree's current answer beside it:

- **Memory, and this is the one that moves.** rustc with LLVM peaks far above
  anything cranelift would have, and a build that overcommits currently dies at
  the faulting task with a SIGBUS-coded exit. Swap was an open decision under
  the Rust-hosted road; under this one it is a Phase 2 prerequisite or a
  per-build memory budget is, and "decide later" stops being available.
- **Subprocesses.** rustc spawns the linker and cargo spawns rustc. `fork`,
  `execve`, `execvp`, `waitpid` and `wait4` exist; `posix_spawn` does not, and
  Rust's `Command` falls back to fork/exec without it, so it is a nicety.
- **The jobserver**, which cargo and rustc use to share a parallelism budget
  across processes — a pipe or a fifo, and a `poll`/`read` that blocks.
- **File locking**, which cargo uses on the target directory and the registry.
  `flock` exists.
- **`mmap` of rlib metadata**, which rustc does for every dependency.
  `MAP_PRIVATE` file mappings with demand paging exist.
- **Disk, for the target directory.** `builddir/target` is 52 GB across 219,895
  files on the host, but that is every variant, every test binary and every
  doc artifact; a single-variant kernel build is a small fraction of it, and
  the honest number is the one a first in-guest build measures rather than one
  extrapolated here.

### Workstream 2.4 — Getting code in and out (**S** for the goal, **M** beyond it)

Off the critical path, and this is a real scope reduction: `Cargo.lock` holds 47
entries of which only nine are third-party (`bitflags gimli libm limine paste
proc-macro2 quote syn unicode-ident unwinding`). Vendoring that is trivial, so
**building SlopOS on SlopOS needs no network at all** — no TLS, no crates.io, no
`git`. Those remain wanted for a general dev machine (there is no TLS anywhere:
`curl` rejects `https://` outright; DNS is one query at a time machine-wide; the
TCP window is capped at 32 KiB by a fixed buffer), but they are Phase 2+
comfort, not a blocker for the goal.

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

## Phase 5 — The toolchain rebuilds itself (not committed)

Out of scope for the goal at the top of this document, which asks that a commit
be *authored, compiled and booted* without a Linux host in the path — not that
the compiler that compiled it was itself compiled here. Recorded as its own
phase so that the difference is visible and nobody widens the goal by accident,
because it is the difference between a C++ *runtime* and a C++ *compiler*, and
that is where the order of magnitude lives.

What it would cost, with the parts that are not obvious named first:

- **The build drivers are C++ too.** LLVM builds with CMake and Ninja, and both
  are C++ programs; LLVM's build also runs Python. So the second-order
  dependency of rebuilding the compiler is a second C++ port plus an
  interpreter, and none of the three is on any other phase's path.
- **clang running in-guest is free, and it is not the hard part.** It arrives
  with `libLLVM.so` in Workstream 2.2's single cross-build. Having the compiler
  is not having the build system, the disk or the hours.
- **Disk and time.** A release LLVM build is tens of gigabytes of objects and
  hours of CPU on a machine with a real scheduler and real I/O. Neither number
  is worth estimating here; what is worth writing down is that both are an
  order of magnitude past Phase 2's, and that Phase 4's bare-metal list is
  where the I/O to support them would come from.
- **The `-Zbuild-std` dependency does not end.** Until
  `x86_64-unknown-slopos` is tier 2 and ships artifacts, every in-guest build
  builds std, which this phase inherits rather than fixes.

The honest framing: Phase 2 makes SlopOS a machine that develops SlopOS. This
phase makes SlopOS a machine that develops its own toolchain, which is a
different and much larger claim, and **nobody has made it.** Redox's January
2026 milestone is a natively *running* rustc and cargo that were cross-built on
Linux: its `mk/prefix.mk` `HOSTED_REDOX=1` branch `wget`s `rust.pkgar` and
`llvm21.pkgar` from its package server rather than building them, its LLVM
recipes hand CMake a host toolchain file for the native tablegen, and the
announcement's own list of what was built on Redox is "relibc, ripgrep,
cbindgen, and the Redox test suite" — no rustc, no LLVM, no GCC. Asterinas is
binary-compatible enough to run an unmodified NixOS userland and is still
*always* cross-built. So Phase 2's claim has been made once; this phase's has
not been made at all.

---

## Open decisions

- [ ] **Does the ABI become binary-compatible, and does the toolchain then stop
      being a port?** SlopOS is Linux-ABI at the numbers, the constants and
      most of the layouts, which buys *source* compatibility. Running prebuilt
      glibc-linked binaries needs the five layouts the POSIX-floor section
      still names as divergent, plus ~70-90 thin entry points; the
      dynamic-linking trio is served by a loader that exists, and `si_addr`
      and `msghdr` left the list on their own when std's PAL started reading
      them. The reference class says it is reachable — Asterinas runs an
      unmodified NixOS userland, gVisor runs unmodified binaries with 277 of
      351 — and the backend decision changed what it is worth: a prebuilt
      rustc is an LLVM rustc, which used to be the objection and is now what
      the tree builds towards anyway, so this became a possible *shortcut
      past* Workstream 2.2 rather than a detour from it. Asterinas is also the
      proof of the ceiling: binary-compatible to the point of an unmodified
      NixOS userland, and still always cross-built. Binary compatibility buys
      running a prebuilt rustc; it does not buy a target that can be a host,
      and it does not remove the proc-macro `dlopen`.
- [ ] **Is `libLLVM` shared or static?** Shared is what upstream ships and what
      the 161 MB `librustc_driver.so` measurement was taken against; static
      removes one `dlopen` but not the proc-macro one, because `rustc_driver`
      is `crate-type = ["dylib"]` and no configuration changes that. It is no
      longer a question about what the loader owes but about how many
      `PT_LOAD`s and how much startup relocation an in-guest rustc pays for,
      which is the first number Workstream 2.2's bootstrap run produces.
      Decide it there rather than on paper.
- [ ] **Does the dev root stay attested?** A machine that rewrites `/usr` while
      building itself un-attests exactly the blocks it changes, and now keeps
      them un-attested across host rebuilds. Decide which paths stay verified
      and what `verity=require` asserts for a workbench.
- [ ] **How does source get in?** The capability is done — `mount(2)` takes a
      named device and `just test-capacity` already builds a 16 GiB volume
      populated from the host with this repository and the pinned sysroot.
      What is left is the *workflow*: a host-built image refreshed per session,
      a 9p/virtiofs mount, or a plain TCP transfer once there is one.
- [ ] **When does swap arrive, and what chooses the victim?** No longer
      deferrable, which is what the LLVM decision changed: rustc with LLVM
      peaks far above anything cranelift would have, and a build that
      overcommits currently dies at the faulting task with a SIGBUS-coded
      exit. Decide between swap plus a reclaim policy and a per-build memory
      budget that makes overcommit not happen — before Workstream 2.3.

**Decided.** C++ runtime: **LLVM's `libc++`, cross-built, libc++ and
libc++abi linked into one `libc++.so`** — settled by building it, and by the
fact that `libstdc++` is not a library you cross-build but one a GCC
cross-compiler emits, which is a second toolchain to pin and keep. What is
still unmeasured is the link `libLLVM` makes against it, which is Phase 1's
exit criterion. Syscall ABI: **Linux x86-64 numbering, one table, a private
range at 1024, and a Linux number obliges the Linux signature.** Rust toolchain:
**LLVM, cross-built from Linux, with the C++ runtime ported to SlopOS** — the
Rust-hosted answer was decided first, then measured against this kernel and
found not to reach it. C is *not* excluded and is now cheaper, because clang
arrives in the same cross-build as `libLLVM.so`, which deletes the
Rust-written C frontend this plan used to owe. Scope: the full in-guest loop,
Phases 1–3, in QEMU. Identity: single-user, uid 0, permanently — so file
ownership and a medium-resident quota ledger stay out of scope and `stat`'s
uid/gid fields exist for layout only. Directory scaling: an in-memory name
index, not an on-disk htree, so `e2fsck` stays the oracle for every image this
kernel writes. Std platform layer: **unix family over a real libc** —
`target-family = ["unix"]`, `env = "slibc"`, a `libc/src/unix/slopos/` module,
std riding its own `sys/pal/unix`, and `slibc/std_pal/` deleted rather than
moved. The rejected alternative was a bespoke PAL over a crates.io ABI crate
(Motor OS's shape), which costs six hard-breaking third-party crates —
`libloading` among them, so an in-guest rustc could not be built at all — as
permanent carve-outs.

---

## Touch list (current paths — verify before editing)

Each landed section above states the invariants its paths carry; this list is
where they live, not a second copy of what they say. An entry marked
*invariant* carries something a change nearby can break without failing to
compile.

| Area | Paths | |
|---|---|---|
| Dynamic loader | `mm/src/elf.rs`, `mm/src/process_vm.rs`, `core/src/exec/mod.rs`, `slibc/src/ld_so/`, `slibc/cdylib/` | *invariant* |
| Terminal | `vt/src/lib.rs`, `terminal-core/src/{input,grid}.rs`, `userland/src/apps/terminal/`, `userland/src/apps/shell/input.rs`, `font/src/{lib,atlas,boxdraw,bitmap}.rs`, `core/src/syscall/font_handlers.rs`, `net-core/src/render.rs` | *invariant* |
| Shell | `shell-core/src/`, `userland/src/apps/shell/{expand,glob,exec,funcs}.rs` | *invariant* |
| Utilities | `userland/src/apps/coreutils/`, `userland/src/bin/coreutils.rs`, the justfile's `coreutils_tools`, `scripts/build_fs_image.sh`, `scripts/gen_initramfs.py` | *invariant* |
| Editor and toolkit | `editor-core/src/`, `userland/src/apps/editor/`, `appkit/src/`, `windowing/src/clipboard.rs`, the compositor's `protocol_pointer_grab` | *invariant* |
| Std/target/unwinding | `toolchain/`, `scripts/make_slopos_sysroot.sh`, `scripts/check_toolchain_pin.sh`, `targets/x86_64-unknown-slopos.json`, `userland/userland.ld` | *invariant* |
| Syscall ABI | `abi/src/syscall/numbers.rs`, `core/src/syscall/handlers.rs`, `scripts/check_syscall_abi.sh`, `scripts/gates/syscall/` | *invariant* |
| Backend and linker gates | `scripts/check_codegen_backend.sh`, `scripts/check_linker_script.sh`, `scripts/gates/{codegen,linker}/`, `targets/x86_64-slos.json`, `link.ld` | *invariant* |
| Storage | `fs/src/ext2/{dirindex,journal}.rs`, `fs/src/verity.rs`, `drivers/src/virtio_blk.rs`, `fs/src/fsreport.rs` | *invariant* |
| C++ runtime | `scripts/make_slopos_cxx.sh`, `scripts/check_cxx_pin.sh`, `toolchain/cxx/PIN`, `slibc/src/{unwind,cxa,math,ctype,stdlib}/`, `slibc/build/`, `userland/cxxtest/` | *invariant* |
| C++ platform | `vendor/unwinding`, `slibc/{staticlib,cdylib,crt0,include}/`, `NOTICE.md` | work |
| Phase 3 install | `scripts/qemu_run.sh`, `fs/src/devfs/mod.rs`, `fs/src/partition.rs` | work |
| Execution boundary | `AGENTS.md` | Phase 3 needs a scoped exception |

Two things are worth naming here because their paths sit apart from the
section that explains them: `libc.so` must keep exporting the seventeen
`_Unwind_*` entry points and must keep being built `-C force-unwind-tables`,
because the first frame of every unwind is one of its own; and `<wchar.h>` is
still not generated at all.
