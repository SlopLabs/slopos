# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

Read that sentence precisely, because one word in it is the difference between
Phase 1 and Phase 4: the compiler must *run* here, not be *built* here. A C++
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
the program that loaded it — and LLVM itself, the library those 161 and 208 MB
are, now compiles for this target — and a bootstrap invocation on Linux
cross-builds rustc, cargo, `rust-lld` and clang *for* it. What is left is
running that toolchain here, and every constant that produced the storage gap
was chosen correctly for an appliance.

**The theme of this plan:** SlopOS's limits are not architectural mistakes,
they are appliance-sized constants and appliance-sized policies. A workbench
needs those quantities derived from the medium (image size, RAM, file size)
instead of frozen at values that fit a test fixture. The work is mostly
*widening under proof*, not redesign, and the compiler bootstrap — the one
exception this plan carried the longest — turned out to be mostly
configuration. The sixteen sections between here and Phase 1 are what has
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
properties share one shell invocation deliberately: `MAX_PROCESSES` was 256 when this
was measured, a run reaches ~170 before this utest, and a spawn per assertion measured 243 held
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

- **The triple is a JSON spec here.** It is a built-in one in the compiler
  fork, the last landed section below, which is what bootstrap's `--host`
  resolves through. Being built in is not free of consequences for this file
  — rustc holds a built-in target to rules it relaxes for a JSON one, and the
  section below says which — but tier 3 ships no artifacts either way, so
  `-Zbuild-std` stays mandatory until tier 2.
- **Upstreaming has not happened.** The patches *are* the PRs — these two and
  the compiler fork's one — in the order the tier policy asks for, and the
  cost is a maintainer name on record and an `MIT OR Apache-2.0` licence on
  the contributed files. Nothing contributed is GPL'd kernel or slibc code.
- **Being a *host* was gated on proc macros, not on the target spec, and the
  gate is now open.** `rustc_driver` is `crate-type = ["dylib"]` and
  `invalid_output_for_target` rejects that outright when `!dynamic_linking`, so
  a static rustc that expands proc macros does not exist. The spec says
  `dynamic-linking: true` and the loader behind it exists; naming the triple
  is the last landed section below.
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
question, which is the opposite of what a development machine is for. The C++
runtime, the libc surface and Phase 1 are the consequence, and both gates keep
their value as the thing that would notice the day either candidate stops
refusing.

---

## A program can be linked at run time

The eleventh thing this plan rests on, and the one Phase 1 could not have begun
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

The twelfth thing this plan rests on, and the one Phase 1 cannot be attempted
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
  the random device and the time-zone database off**, and turning them off
  compiles them out rather than stubbing them, so `<iostream>`, `<regex>`,
  `<locale>` and `<fstream>` are unusable. Redox's libc++ is configured the
  same way. What that cost at the time was `wcstof`, `wcstold` and
  `nl_langinfo`, none of which existed here; the section below closed all
  three, and what is left is the `*_l` family and `<wctype.h>`.
- **`long double` is `strtold` and nothing else.** It is x87 80-bit on this
  target, Rust has no type for it, and the System V ABI returns it in `st(0)`
  — which no Rust signature can name — so the one entry point libc++ needs to
  build is two instructions of assembly that widen `strtod`'s `double`. It
  carries `double` precision, stated. The rest of the family is the section
  below; a C++ `<cmath>` lowers those overloads to `__builtin_*l`, which until
  then only became a call to a missing symbol if a program used one.
- **`strtod` does not read hexadecimal significands.** The scan stops at the
  `x`, which is C89's reading of it and a refusal a caller can see in `endptr`
  rather than a wrong value. The section below is where C99's reading lands.
- **The C++ runtime reaches the tests image only — the unwinder does not.**
  `libc++.so` as a file and `libc++.a` inside `cxx_static_probe`; the shipped
  appliance root runs no C++ program, and a megabyte of runtime nothing links
  belongs on the dev disk Phase 1 builds rather than in the image `just boot`
  attests. `libc.so` is a different matter: the `unwinder`
  feature is on for it unconditionally, so every image's C library carries
  `unwinding` and the DWARF reader behind it, and every process registers the
  frame finder at startup. That is what `--gc-sections` cannot drop while
  startup names the finder, and building a second, feature-off `libc.so` for
  the shipped image would mean two C libraries that differ between images —
  a worse hazard than the bytes. Measured: 412 384 against 241 376 before.
- **The C++ programs are the two probes and no more.** Nothing in the system
  is written in C++ and nothing should be; the runtime exists so that a
  cross-built LLVM can run, which is Phase 1's first measurement.
  `cxx_static_probe` is not a second feature — it is what keeps `libc++.a`,
  `libc.a`'s unwind tables and the finder's `AT_PHDR` road from being three
  things nothing has run.

---

## The libc surface is complete

The thirteenth thing this plan rests on, and the last one the compiler stands
on: a C program compiled against SlopOS's own headers and linked against
its own C library runs. `/bin/libc_probe` is the standing proof — the tree's
only pure-C program, cross-compiled by clang with `-nostdlibinc -isystem
slibc/include` and statically linked against `libc.a`, working through
seventeen ordered checks and exiting with the number of the first failure:
`setjmp` and `longjmp` with a non-volatile local live across them, `sigsetjmp`
restoring a mask out of a `jmp_buf` filled with `0xff`, the civil calendar,
`strftime` against a `tm_zone` no format reads, `clock`, `printf`'s float
conversions and `%Lf`, `scanf`'s, the C locale and
`nl_langinfo`, `qsort` past its insertion-sort cutoff and under a comparator
that lies, `strerror` from two threads at once, hexadecimal `strtod`, the
multibyte conversions and their `EILSEQ` write-back, the wide numeric family,
the `long double` family's precision and its x87 stack discipline, and a
function-pointer table that makes the linker resolve all 56 of those entry
points. Before this, everything slibc exported was exercised from Rust.

What it rests on, in case a later phase disturbs it:

- **The arithmetic is a crate and the crate is host-testable.**
  `slibc-core` is `#![no_std]`, `#![forbid(unsafe_code)]`, allocation-free and
  answers by value or into a caller-owned slice: the civil calendar,
  `strftime`, the UTF-8 conversion state machine, hexadecimal float scanning
  and the C float formatter, 2 265 lines against 58 cases under
  `just test-host`. The split `shell-core`, `terminal-core` and `editor-core`
  already draw, drawn once more — and the ABI-bound halves cannot follow it,
  so the x87 shims, `setjmp` and the locale globals are proved in the guest
  instead.
- **`jmp_buf` is derived from the ABI and then pinned.** Eight callee-saved
  slots and the continuation, glibc's and musl's 200-byte
  `struct __jmp_buf_tag[1]`, with `size_of`, `align_of` and three offsets held
  by `const _: () = assert!`. `setjmp` does not save the signal mask — glibc's
  own header defines `setjmp` as `_setjmp` — but it does zero the flag
  `siglongjmp` reads, or a buffer with automatic storage installs a mask out
  of stack residue. `__setjmp` is exported and deliberately undeclared:
  clang carries `returns_twice` for `setjmp`, `_setjmp` and `sigsetjmp` and
  not for that spelling, so a C caller of it would be miscompiled.
- **The `long double` family is the `double` family, entry for entry.** 56
  against 56, in two stated tiers: 28 are exact at the x87's 64-bit
  significand because the hardware has the operation or the answer is bit
  work, and 28 narrow to `double` and widen back. `ldexpl` and its two
  siblings apply the shift as *two* `FSCALE`s, because the x87's exponent
  range is 32 830 wide and one step of at most 32 768 cannot saturate — a
  single step answered 8.4e-4933 where glibc answers 0.
- **The C locale here is UTF-8-coded, which is musl's reading.**
  `MB_LEN_MAX` is unconditionally 4 (clang's freestanding `<limits.h>` was
  answering 1, so `MB_CUR_MAX 4 > MB_LEN_MAX 1` was a live C11 violation),
  `MB_CUR_MAX` derives from it, and `nl_langinfo(CODESET)` answers `UTF-8`. A
  program that reads `MB_CUR_MAX == 1` as "the C locale" takes the multibyte
  path here.
- **The `LC_TIME` strings are one table.** `nl_langinfo`'s day, month and
  format answers are built at compile time out of `strftime`'s own tables, so
  the two cannot disagree; a second table would have needed a per-entry assert
  to say the same thing.
- **`qsort` allocates nothing and survives a comparator that lies.** Insertion
  sort to 16, median-of-three above it and a ninther past 128, heapsort at
  twice the log, and a 64-entry range stack that cannot fill because only the
  larger half is ever pushed — so the worked range is at most `n >> t` at
  depth `t`. Every index is derived from the range's own bounds rather than
  from the pivot, which is what makes an inconsistent comparator a wrong order
  rather than an out-of-bounds read.
- **`printf` has float conversions, and that is not a footnote.** Before this
  an unrecognised conversion echoed `%f` and consumed no argument, so a single
  `%f` desynchronised every later conversion in the same call — worse than
  absent. LLVM's `raw_ostream` prints a double by building `%.<prec>e` at run
  time and handing it to `snprintf`, and reads the return value as the length
  that *would* have been written. Digit generation is `core::fmt`'s, which is
  correctly rounded and works in `no_std`; what is written here is C's
  spelling — the two-digit signed exponent, `%g`'s style rule applied after
  rounding, its trailing-zero removal, and `%a` straight from the bits.
- **The calendar is one implementation.** The era/day-of-era decomposition in
  `i64` with Euclidean division, which is also what the CMOS driver computes;
  the two copies in `coreutils` were migrated onto it, and a latent
  negative-day bug in the inlined one died with it. There is no timezone
  database: `localtime` is `gmtime`, `mktime` is `timegm`, and each pair
  shares one body.
- **The headers are generated, and the generator learned three things.** An
  array-typed export renders (`char *tzname[2]`), a `pub static` whose type
  contains `[T; N]` no longer stops at the wrong `;`, and a macro
  metavariable is not an export. `slibc/include/**` stays build output; the
  C probe compiles it under `-Wall -Wextra -Wsystem-headers -Werror`, and
  `-Wsystem-headers` is load-bearing because `-isystem` silences exactly the
  diagnostics the probe exists to catch.

**What this deliberately did not do.**

- **`strtold` and `wcstold` carry `double` precision**, and so do the 28 Tier B
  entries of the `long double` family and `%Lf`. Each is two instructions that
  widen an `f64` onto `st(0)`; an 80-bit decimal parser and an 80-bit
  elementary-function library are not here. `hypotl` is the one composed Tier A
  answer and is within 1 ULP rather than exact.
- **No body quiets a signalling NaN**, where C17 F.10 p11 asks for the quiet
  form: `fld tbyte` does not, there is no `<fenv.h>` to observe the invalid
  flag with, and an sNaN can only arrive from punned bits.
- **`<wchar.h>`, `<langinfo.h>` and `<locale.h>` were C-only in this tree,**
  because libc++ was built with localization and wide characters off and a C++
  translation unit reached its `#error` first. The estimate for turning them
  on — 120 names, measured against `libcxx/src/locale.cpp` — was right about
  the number and wrong about the set. "LLVM builds for this target" below
  carries what it actually cost.
- **`setlocale` refuses every locale but `C`.** `""`, `"C"` and `"POSIX"`
  select it, `NULL` queries it, anything else answers `NULL` with no state
  change. Answering `en_US.UTF-8` while behaving as the C locale is a lie the
  caller cannot detect.
- **`gmtime`, `localtime`, `asctime` and `ctime` keep POSIX's shared
  statics** — one `struct tm` and one 26-byte buffer per process. The `_r`
  forms are what a thread should call. `strerror` went the other way and is
  per-thread, because two threads sharing one message buffer is the bug every
  libc that tried it had.
- **`wcstod` and its family take the allocator lock** for a subject sequence
  past 512 bytes, which no `strto*` does, so they are not async-signal-safe.
  POSIX requires that of neither family, and truncating a 600-digit number
  instead would be a wrong answer rather than a slow one.
- **No wide stdio**, no `fwprintf`, no stream orientation, on the grounds that
  nothing asked for one. libc++'s `std::wcin` does, which the section below
  found out by building it.
- **The tests image gained a process.** `libc_abi_test` spawns the probe, so
  the post-userland `process` peak is 255 against a `MAX_PROCESSES` of 256.
  That is the first appliance-sized constant this plan has actually pressed
  against, and the next utest that spawns needs the constant raised rather
  than the gate.
- **`libc.so` grew by 118 KB**, 412 384 to 530 624, for roughly 180 entry
  points. `qsort` alone costs more than the whole 56-entry `long double`
  family, which is the shape of a monomorphised sort against 56 naked stubs.
  Every image carries it, because a C library that differs between images is
  the worse hazard.

---

## The target is a built-in target

The fourteenth thing this plan rests on: `x86_64-unknown-slopos` is a
**built-in rustc target**, not only a JSON file. A JSON spec is enough to
build *for*, and it is not enough to build rustc *for*: bootstrap resolves
`--host` through the compiler's own built-in list, and `rustc_driver` is
`crate-type = ["dylib"]` in its own `Cargo.toml`, so a host rustc is a
dynamically linked compiler for a triple rustc can name.
`toolchain/compiler/0001-slopos-target.patch` is that name — 258 lines over
twelve files of the pinned nightly's own sources — and
`scripts/check_rustc_target.sh` is the standing proof: it holds the built-in
spec to `targets/x86_64-unknown-slopos.json` field for field and then runs
rustc's own per-target test against it.

What it rests on, in case a later phase disturbs it:

- **The compiler fork is a third tree, and it stamps its own inputs.** The
  sysroot the std and libc forks live in is a clone of a *built* toolchain, so
  a patch to rustc's sources has nowhere to land in it.
  `scripts/make_rustc_src.sh` materialises `third_party/slopos-rustc-src`
  instead — 265 MB fetched against a checksum in `toolchain/compiler/PIN`,
  656 MiB on disk, 17 s — and `check_toolchain_pin.sh` now grades two trees
  rather than one, each against the stamp of the overlay half it was built
  from. Sharing one stamp would mean every compiler-fork edit re-extracting a
  source tree and every std edit re-checking one it did not touch.
- **What the tree does not carry is stated, because the phase below builds
  from it.** `vendor/` (2.4 GB of crates.io copies) and `.cargo/`, which
  redirects crates-io at it, are dropped together, so the bootstrap run is
  not an offline one; `src/llvm-project/` (1.4 GB) is dropped because the C++
  this tree needs is pinned in `toolchain/cxx/PIN` and a bootstrap takes LLVM
  from there or from `download-ci-llvm`. Putting the three back is what an
  offline or self-contained build costs.
- **A built-in target may not say `Os::Other`.** `os`, `env` and `arch` are
  enums with an open `Other` arm that `check_consistency` refuses for built-in
  targets, so the patch adds `Os::Slopos` and `Env::Slibc` — which is also
  what puts `slopos` and `slibc` in the `target_os`
  and `target_env` values `--check-cfg` knows, and why the patch carries two
  blessed snapshots of those lists, the arms librustdoc's *exhaustive* match
  over both enums needs — without which the compiler no longer builds, which
  a gate that compiles `rustc_target` alone cannot see and one match-arm grep
  can — and tidy's per-target assembly revision, alongside the base opts, the
  target module, the `supported_targets!` line and a tier-3 doc page. The JSON's `vendor: "slopos"` went the other way and
  is **deleted**: the tuple's own vendor field is `unknown`, nothing reads
  `cfg(target_vendor)` here, and claiming it would have blessed four more
  snapshots to say something untrue.
- **The JSON's `llvm-target` gained its object-format component.** It said
  `x86_64-unknown-none` where upstream spells an OS LLVM does not know
  `x86_64-unknown-none-elf`; the kernel spec was fixed for that in the backend
  spike above, where `target-lexicon` answering `BinaryFormat::Unknown` ICEs
  cg_clif, and the userland spec was left behind. A no-op under LLVM, which is
  why it survived, and free to correct while both specs were moving anyway.
- **A new target is not one the *stage0* compiler knows.** bootstrap validates
  every `--host` and `--target` against the target list of the compiler it
  starts from, before it builds anything, so a triple that exists only in the
  tree being built fails sanity rather than bootstrapping. Upstream's answer
  is `STAGE0_MISSING_TARGETS`, a list that is empty whenever no new target is
  pending; the patch puts the tuple in it, and it comes back out the day a
  stage0 that knows the triple ships.
- **The JSON spec was internally inconsistent, and becoming a built-in target
  is what said so.** It declared `dynamic-linking: true` with
  `relocation-model: static`; rustc relaxes that pairing for JSON targets and
  rejects it for built-in ones, because a target that allows dynamic linking
  must be `pic`. The permission is load-bearing — `libc.so` is a `cdylib` and
  `rustc_driver` is a `dylib` — so the spec says `pic` and the static images
  pin `-C relocation-model=static` on the build line, beside the `crt0.o`, the
  linker script and the `--emit-relocs` they already pin there. Measured, against a build of the
  same tree under both specs: `.text` and `.rela.text` identical to the byte,
  no `.got` in either, the residual difference string-merge packing behind a
  changed `-C metadata` hash.
- **One spec, and a gate rather than a convention.** Two files now describe
  one machine, and a disagreement between them fails to compile nowhere: a
  cross-built toolchain would produce binaries for a slightly different target
  than the tree tests. The comparison is `Target::to_json()` on both sides —
  rustc's own normalisation, every field, defaults elided — plus the two facts
  the fork exists for: the tuple is in `TARGETS`, and the target still allows
  dynamic linking.
- **`host_tools` is a distribution fact, not a capability.** The metadata says
  `host_tools: false` and `tier: 3`, and the only thing in the compiler tree
  that reads that field is `src/tools/build-manifest`, which splits tier-1 and
  tier-2 targets into the dist manifest's host list and its target list on
  exactly that field and never reaches a tier-3 one. Redox ships `Some(false)`
  and hosts a native rustc.
- **The gate builds `rustc_target`, not a compiler.** Twenty-eight seconds
  cold and 1.3 s warm, at 1.1 GB of probe and test objects under
  `builddir/gates/` that `just clean` removes, against the hours a stage-1
  build would cost — which is what makes "is the spec still real" a question
  CI can ask every run. The source tree it needs is a 265 MB fetch, so the
  gate reports `skipped` without one and the CI job that materialises one
  passes `--require`.

**What this deliberately did not do.**

- **Nothing was upstreamed.** The patch is PR-shaped — the doc page, the
  `SUMMARY.md` entry, the assembly revision tidy demands — because that is the
  cheapest way to keep it small, not because a PR is open. Tier 3 ships no
  artifacts, so `-Zbuild-std` and `-Zjson-target-spec` stay on every build
  line until tier 2 whatever happens to the PR.
- **One fixture is left un-extended, deliberately.**
  `tests/rustdoc-html/doc-cfg/all-targets.rs` enumerates every `target_os` and
  `target_env` by hand and asserts the rendered prose, and it does not derive
  that list from the compiler — so it keeps passing with `slopos` missing,
  while an entry added with a wrong expectation string would make it fail.
  Regenerating it needs a built rustdoc, which this gate deliberately does not
  pay for, so the list is one target short until the PR is prepared against a
  tree that can run `x test rustdoc-html`.
- **Two host flips are the bootstrap run's, and nobody's prior art covers
  both.** The spec is `panic-strategy: abort`, and rustc is not a program that
  can abort on a fatal diagnostic: `FatalError::raise` is a `resume_unwind`
  and `catch_fatal_errors` is a `catch_unwind`. And `rustc_driver`'s dylib is
  a hard-coded crate type. Motor OS's fork answers the second and not the
  first: `compiler/rustc_driver/Cargo.toml` on `moturus/rust@motor-os-rustc`
  is `["dylib", "rlib"]`, carrying a comment that rustc drops the dylib crate
  type on a target without dynamic linking and links the driver statically
  from the rlib — while `spec/base/motor.rs` there still sets
  `PanicStrategy::Abort`, so what that fork ships is a native rustc that
  aborts on a fatal diagnostic. Whether SlopOS unwinds in userland or patches
  the driver is a measurement of the first bootstrap run rather than a
  decision to take here; what the tree must not do is answer it in the
  built-in spec alone, because the JSON one is what the system's own binaries
  are built with.
- **No bootstrap invocation.** This section makes `--host=x86_64-unknown-slopos`
  a triple rustc can resolve. Whether bootstrap then *finishes* is the first
  workstream below, and it is the one that produces numbers rather than
  patches.

---

## LLVM builds for this target

The fifteenth thing this plan rests on, and the one Phase 1 was actually
blocked on: **LLVM cross-compiles for `x86_64-unknown-slopos`.** Not a
translation unit and not a probe — the Support library, the IR, the MC layer,
the X86 code generator, the pass managers and LTO: 1 345 build steps, no
errors, against slibc's own headers and the C++ runtime this tree builds.
`scripts/check_llvm_port.sh` is the standing proof, and it grades
`LLVMSupport` alone because that is where a port lives: `Unix/Path.inc`,
`Unix/Process.inc`, `Unix/Program.inc` and `Unix/Signals.inc` are the files
that name a libc, `raw_ostream.cpp` and `ConvertUTF.cpp` the ones that name a
C++ library, `Triple.{h,cpp}` where the port's own enumerator lives, and the
other twelve hundred objects are portable C++ over them. Four of the port's
ten files; the clang half needs a clang, and the driver in it is graded
separately by `scripts/check_clang_driver.sh`. 47 s cold on four cores, 1.5 s
warm.

The estimate this replaces is the one the workstream below carried, and it was
wrong in the direction that mattered. It priced *localization* at 120 libc
entry points and took that for the whole of it. Five libc++ options were off,
LLVM reaches four of them, the C library underneath was missing four entire
headers, and two of its existing declarations were wrong in a way only a C++
compiler ever says out loud. The count came out at 120 exactly, over a
different set.

What it rests on, in case a later phase disturbs it:

- **The C++ runtime is configured for a compiler rather than for a probe.**
  Localization, wide characters, `<filesystem>` and the random device are all
  on: `raw_os_ostream.cpp` reaches `<ios>`, `ConvertUTF.h` names
  `std::wstring` unconditionally, eleven files under `clang/` include
  `<fstream>`, and `LockFileManager.cpp` constructs a `std::random_device`.
  Every one of those was a hard `#error` or an undefined template rather than
  a link failure, and every one was reachable without cross-building anything.
  The time-zone database stays off — nothing in LLVM or clang asks for one,
  and there is no zone data on this system to answer from.
- **libc++ classifies characters out of its own table.**
  `_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` is upstream's alternative to the glibc
  road, where the table is `__ctype_b_loc()`'s and `ctype_base::mask` is
  `_ISspace` and its eleven siblings — a second classification of the same 128
  characters, living in the C library, which libc++ `#error`s for on any
  platform that supplies neither. It is an *ABI* flag, because it decides the
  width and the bit values of a type passed by value: a consumer compiled
  without it disagrees with the runtime about `ctype_base::mask`. That is why
  it is written down once, in `make_slopos_cxx.sh --print-abi-flags`, and why
  `build_userland.sh` and the gate ask for it rather than restating it.
- **The random device is `getentropy`, not `/dev/urandom`.** libc++'s default
  opens a device SlopOS's devfs does not have; `_LIBCPP_USING_GETENTROPY`
  takes the entry point instead, which is one `getrandom` with POSIX's
  256-byte cap and no blocking path to get stuck in.
- **120 C entry points, in four groups.** POSIX-2008's locale objects
  (`locale_t`, `newlocale`, `duplocale`, `freelocale`, `uselocale`) and the 53
  `_l`-suffixed functions that take one; `<wctype.h>`, which did not exist at
  all — its eighteen classifiers and transforms; the sixteen wide stdio
  entry points `<wchar.h>` has always declared and this tree never had, C99
  §7.24.2 bar the wide `scanf` family; and the ordinary POSIX names that were
  simply absent — `strdup`, `strndup`, `strcoll`, `strxfrm`, `strsignal`,
  `isascii`, `toascii`, `rand`, `srand`, `_Exit`, `asprintf`, `vasprintf`,
  `vsscanf`, `vfscanf`, `vscanf`, `fseeko`, `ftello`, `getentropy`,
  `getpwnam_r`, `pathconf`, `fpathconf`, `utimes` and `getsid`, together with
  `<inttypes.h>`'s own six — `imaxabs`, `imaxdiv`, `strtoimax`, `strtoumax`,
  `wcstoimax` and `wcstoumax`. Four headers
  were new with them: `<wctype.h>`, `<inttypes.h>`, `<endian.h>` and
  `<sysexits.h>`, every one of which LLVM includes.
- **The `_l` family is an answer, not a placeholder.** SlopOS has exactly one
  locale, so each `_l` function is its base function with the handle
  discarded — musl's shape, for musl's reason. `newlocale` still refuses a
  name `setlocale` refuses, so a program asking for `en_US.UTF-8` is told no
  rather than handed the C locale wearing that name, and `uselocale` really
  does keep a per-thread handle (in the TCB, with the pre-TLS static fallback
  `errno` uses) so that querying it answers what was set.
- **`getsid` is a new syscall.** Number 124, Linux's, mirroring `getpgid`
  against the session id the task already carries. `LockFileManager.cpp` asks
  whether a lock's owner is alive, and there was no way to answer.
- **Two C declarations were wrong, and only a C++ compiler said so.**
  `struct sigaction` had one member, `sa_sigaction`, typed as the `size_t` the
  `libc` crate spells it as — so `Handler.sa_handler = f` did not compile, in
  three of LLVM's Support files. The header now renders that slot as the union
  POSIX describes, with both names and both function-pointer types, which is
  glibc's shape without glibc's global `#define`. And `S_ISFIFO` was spelled
  `S_ISIFO`: the generator derives each test from its `S_IF*` constant, and
  the FIFO is the one type whose POSIX name is not that derivation.
- **`struct stat` carries POSIX-2008's three `timespec` members.** `st_atim`,
  `st_mtim` and `st_ctim`, with the C89 spellings as the `#define`s glibc and
  musl both use. libc++'s `<filesystem>` reads them by those names; the
  previous `time_t` plus `long` pair was the same bytes under names no C++
  standard library knows.
- **The port of llvm-project is a patch, and it is the size a port is.** 102
  lines over six files at this point — the clang driver takes it to ten, in
  the section below: the two places LLVM dispatches on the OS with no
  default a new one can take (`<endian.h>` versus `<machine/endian.h>` in
  `ADT/bit.h`, and `statvfs.f_flag` versus a BSD `f_flags` and `MNT_LOCAL` in
  `Unix/Path.inc`), plus the `Triple` entry and the clang target that make
  `__slopos__` a macro a compiler predefines. `scripts/make_slopos_llvm_src.sh`
  materialises the tree (the same pinned tarball the C++ runtime is cut from,
  extended to all of `llvm/` and `clang/`: 1.5 GB on disk, 11 s), and the
  patch is pinned by checksum in `toolchain/cxx/PIN` — a fourth fork, graded
  beside the tree it applies to for the reason the compiler fork is.
- **compiler-rt is this C library's job.** x86-64 codegen calls out for
  128-bit arithmetic, and there is no libgcc here to take those calls from.
  `libc.a` already carried them, because a staticlib links `compiler_builtins`
  whole; `libc.so` cannot publish them, because rustc gives a cdylib a version
  script that localises everything but the crate's own exports. So
  `libbuiltins.a` is a third artifact — the same routines, built
  `relocation-model=pic`, read last on the C++ runtime's link line, which is
  how every other platform takes compiler-rt: out of an archive that yields
  only the members still undefined by the time it is reached.

**What this deliberately did not do.**

- **No bootstrap.** This section makes LLVM a library that compiles for
  SlopOS. Building a *compiler* is the workstream below, and what stands
  between the two is cargo rather than LLVM: bootstrap builds cargo before
  anything for the host triple, and cargo's manifest pulls six `-sys` crates
  that build C libraries, none of them optional. The workstream below takes
  that as a fifth fork rather than five C ports.
- **No clang driver.** The patch adds the target that predefines the macros;
  it adds no `ToolChains/SlopOS.cpp`, so a cross-built clang cannot yet be
  handed a bare `-o` and asked to find `crt0.o` and `-lc` by itself. Every
  link line in this tree states those explicitly, which is why nothing needed
  it yet and why the first in-guest `cc` will.
- **`__slopos__` is still a command-line macro.** The port teaches a
  *cross-built* clang to predefine it; the host clang that runs the cross
  build is not that clang, so `check_llvm_port.sh` passes `-D__slopos__` and
  says why. The first toolchain built from this tree is what retires the flag.
- **The gate builds two libraries, not LLVM.** The full set above was built
  once, by hand, to find out whether it would; what CI can afford every run is
  the portability surface and the Triple. A gate that compiled all of LLVM
  would be measuring the host's core count. Clang's half of the port is the
  residual: nothing but `git apply --reverse --check` holds
  `SlopOSTargetInfo`, and reaching it means building a clang.
- **The gate borrows the host's `llvm-tblgen`.** It builds no host tools, and
  the `.inc` files tablegen emits are data tables rather than code, so a host
  tool of the same major serves. A build that needs its own would be building
  a second LLVM first.
- **The narrow `scanf` engines grew what the new header advertises.**
  Shipping `SCNo*`, `SCNx*` and `SCNi*` meant the engines behind them had to
  exist: there was no `%o` arm in either, no `%x` arm in the stream one, and
  `%i` read decimal whatever its subject's prefix said. Assignment suppression
  and the maximum field width were missing too, which mattered more — `%*d`
  aborted the whole scan rather than converting and discarding, and `%31s`
  was no bound at all, so a caller's own overflow mitigation did nothing. All
  six conversions now share one subject reader over the existing `Cursor`,
  which is what had let the two engines disagree about `%x` in the first
  place.
- **`libc_probe` covers the new surface.** 120 entry points arrived with no
  behavioural test between them; the C probe now carries six more checks —
  the locale objects and the `_l` identity, `<wctype.h>`, wide stdio, the
  `<inttypes.h>` six and their format macros, the scan conversions above, and
  the ordinary POSIX names. Every expectation in them was first run against
  glibc under a UTF-8 locale, so a failure on SlopOS is slibc's and not the
  test's.
- **No wide `scanf`.** `fwscanf`, `swscanf` and the `v` forms are absent and
  undeclared. The wide `printf` family transcodes its template to UTF-8 and
  runs the narrow engine — C gives the two templates the same conversions, so
  the transcode is the whole difference — and the scanning direction consumes
  its template and its stream together, which makes it a second parser rather
  than a second spelling. Nothing links against it.
- **`<inttypes.h>` is literal C rather than a generated contract.** Its format
  macros expand to string literals that the header generator's
  `#define NAME (value)` rendering would parenthesise into a syntax error, and
  its six entry points are spelled in the compiler's own `intmax_t`. A header
  spec's `raw` block is emitted verbatim and never reaches `build.rs`'s
  signature check, which iterates `extra` — so these six join the four
  `long double` prototypes, `imaxdiv_t`, `LC_GLOBAL_LOCALE`, the three
  `st_*time` defines and all of `<endian.h>` as declarations nothing grades.
  That is why each `PRI`/`SCN` macro is the compiler's own
  `__<TYPE>_FMT<conv>__` predefine rather than a length modifier written down
  here: a hand-written table says what the author believes `int_fast16_t` to
  be, and clang makes that one a `short`. `libc_probe` now exercises the
  macros against their own types instead, which is the nearest thing to a
  check that a verbatim block can have.
- **`libc.so` grew by 40 KB**, 530 624 to 571 696, and `libc++.so` is now
  1 762 808 bytes — four options' worth of C++ library, and the locale facets
  are most of it. The runtime is on the tests image only; the C library is on
  every image, and a libc that differs between images is the worse hazard.

---

## The toolchain is cross-built and lands on a dev disk

The sixteenth thing this plan rests on, and the one Phase 1 was named for:
**one bootstrap invocation on Linux, `--build=x86_64-unknown-linux-gnu
--host=x86_64-unknown-slopos`, builds rustc, cargo, `rust-lld`, clang and
`libLLVM.so` for SlopOS, and a dev disk is where they land.** What the tree
carries is that invocation and everything it needs — six pinned forks, a
compiler wrapper, a sysroot, a volume and four gates — not its output, which
is hours of CPU and tens of gigabytes and belongs on a disk rather than in a
repository. Nothing in that sentence is novel — it is how every cross-hosted
Rust distribution is produced — and everything in it depends on the fifteen
sections above: every artifact is dynamically linked, throws, calls a C
library, is built for a triple the compiler can name, and is *made of* a
library that compiles for that triple.

Four gates are the standing proof, because the build itself is hours of CPU
and what can be graded every run is everything up to the first object.
`scripts/check_bootstrap_config.sh` drives bootstrap's own dry run — which
validates the config, resolves `--host` through the built-in target list and
walks the whole step graph — 16 steps, ending at a stage2 rustc, cargo and
std installed for the target — and then compiles and links a C program and a
shared C++ object with the generated wrapper, grading the interpreter, the
`NEEDED` entries and the image type of what came out.
`scripts/check_cargo_fork.sh` holds cargo's `network` cut to dropping every C
library and still compiling. `scripts/check_clang_driver.sh` drives a real
`clang::driver::Driver` at the triple and grades the link job's argv against
the line `build_userland.sh` writes by hand. `just test-devdisk` is the
fourth: the guest mounts the volume, reads the staged inventory back and
mounts it again.

**Three things this workstream priced wrong, and the measurements that
corrected them.** The estimate said bootstrap builds cargo before anything for
the host triple; it does not — cargo is an *extended* tool, gated on
`build.extended` and `build.tools`, and a stage2 rustc does not depend on it,
so the fork is needed for the goal and not for the ordering. The estimate
said six `-sys` crates, none optional; `openssl` and `openssl-src` were
already `optional = true` upstream, the five that were not are `curl`,
`curl-sys`, `git2`, `git2-curl` and `libgit2-sys`, and there is a *seventh* C
library the list missed — SQLite, through `rusqlite`'s `bundled` feature. And
the cut was priced at seventeen of cargo's 260 source files; the patch is 23
files and 385 added lines against 43 removed.

What it rests on, in case a later phase disturbs it:

- **cargo is the fifth fork, and it shares the compiler fork's tree.** The
  rustc source tarball carries cargo at `src/tools/cargo`, so
  `toolchain/cargo/0001-slopos-cargo.patch` lands there and the two forks
  share one materialised tree and one stamp — and not one PIN, because this
  patch is a PR to rust-lang/cargo and the others are PRs to rust-lang/rust.
  The feature is `network`, on by default, and what hangs off it is curl,
  libgit2 and the five pure-Rust crates only their code paths reach. Twenty
  crates leave the dependency closure, every C library among them. The cut
  was established by building that cargo and using it: it builds a vendored
  workspace and refuses a registry with a named reason rather than a link
  error. What the gate holds every run is cheaper — the two closures and a
  `cargo check` of the offline side — because a `cargo build` of cargo is
  minutes.
- **SQLite stays, and three libc names are the whole reason.** The seventh C
  library is the cheapest of them — one amalgamated file, no build system,
  and `cc` takes its target from `CARGO_CFG_TARGET_*`, which cargo sets for
  any triple. Compiling it for `x86_64-unknown-slopos` against slibc's
  headers stopped on exactly three names: `strspn`, `strcspn` and
  `FILENAME_MAX`. With those it compiles clean, so the alternative — stubbing
  1 840 lines of `global_cache_tracker` — was never paid.
- **The fork is a cut, not a feature flag over dead code.** Without
  `network`, `SourceId::load` answers an `UnavailableSource` for a git or
  remote-registry id rather than an error, because source replacement *loads*
  the original just to ask whether it checksums and whether it needs a
  precise version. Answering those two constants and refusing everything else
  is what makes a vendored build work at all; returning an error there broke
  it, which is how the case was found.
- **bootstrap needed two patches of its own, and running it is what found
  them.** `build.tool.<name>.features` can only *add* features, so
  `toolchain/compiler/0002` adds the `default-features` key that lets a host
  target drop cargo's defaults. And bootstrap maps a cross target's triple to
  a `CMAKE_SYSTEM_NAME` by hand: an unrecognised one prints a note, falls back
  to `Generic` and exits 0 — losing `LLVM_ON_UNIX` and with it every
  `Unix/*.inc` file the LLVM port patches. `0003` is the arm, and `Linux` is
  the value, for the same reason `make_slopos_cxx.sh` gives.
- **The clang driver is in the port; the wrapper stands in until a clang
  built from the port runs the build.** `toolchains::SlopOS` is
  `toolchain/llvm/0002-slopos-clang-driver.patch` — `crt0.o` first,
  `--image-base=0x400000`, `--dynamic-linker=/lib/ld-slopos.so.1`,
  `--eh-frame-hdr`, `-z now`, `-L<sysroot>/lib -lc`, `libbuiltins.a` last.
  The host clang that runs the cross build is not that clang, so
  `bootstrap_slopos_toolchain.sh` writes the same policy as a shell wrapper
  and hands it to CMake and to every build script.
- **The wrapper names two triples, and that is the whole reason it exists.**
  Compilation must name SlopOS, or the preprocessor defines `__linux__` and
  LLVM takes the `/proc/self/exe` and `sched_getaffinity` paths this system
  has not got. Linking must not: the host clang has no SlopOS toolchain, so
  for that triple it hands the link to `gcc` — a host GCC
  `scripts/cxx_host_tools.sh` deliberately does not require, and the host's
  library directories on the line. So a link invocation compiles its sources
  for SlopOS first and links the objects under the Linux triple with
  `-fuse-ld=lld`.
- **`--sysroot` is what stops the host's libc answering.** Even under the
  Linux triple clang adds its own `-L` paths, and slibc is one library — no
  separate `libm`, `libdl`, `libpthread` or `librt` — so a probe for one of
  those would otherwise resolve against glibc and produce a binary that dies
  on SlopOS. The sysroot confines the search, and empty archives answer the
  four names the way a musl-derived sysroot does. Measured: `-lm` is a link
  error.
- **The LLVM port exists twice, and running the build is what said so.**
  `make_rustc_src.sh` drops `src/llvm-project` because no gate needs 1.4 GB
  of C++, and the pinned llvm-project the C++ runtime is cut from is not a
  substitute: rustc links a C++ shim against one specific LLVM API, and
  `download-ci-llvm` serves the build triple only. So a real run unpacks that
  subtree from the tarball already on disk — and then finds that
  `toolchain/llvm/`'s hunks do not apply to it, because the two trees are
  18.1.8 and 23.1.1. `toolchain/llvm-rustc/` is the same port against the
  second: the same `Triple` entry, the same two OS dispatches, the same
  `toolchains::SlopOS`, re-derived where five years of API moved under it.
  Two copies of one port is the cost of a C++ shim pinned to a compiler's own
  LLVM, and the one thing that must not happen is for them to diverge.
- **The std and libc forks land in two trees for the same reason.** The
  sysroot's `library/` is what `-Zbuild-std` reads; the rustc source tree's
  is what bootstrap builds the target's std out of, and it arrives with no
  `library/libc` at all — `make_slopos_sysroot.sh` is what unpacks the pinned
  crate. Staging it is the bootstrap script's, stamped with the sysroot's own
  digest so an edit to either fork re-stages this one.
- **The materialised trees are outside this repository's cargo workspace.**
  `Cargo.toml`'s `exclude` carries `third_party`, because cargo resolves
  `src/bootstrap/Cargo.toml` against the nearest ancestor workspace that
  claims it and found SlopOS's. Bootstrap refused to build at all — the same
  class of failure as `git apply` resolving a patch's paths against the
  repository root, and caught the same way, by running the thing.
- **The dev disk is a fifth volume and the guest finds it by what is on it.**
  `DEV_DISK_IMG` attaches `fs/assets/ext2-devdisk.img` as `virtio-disk4`,
  after the capacity disk so `CAPACITY_IMG`'s `vdd` keeps its letter. The
  marker at the volume root records every staged path's size *read back out
  of the image* rather than off the stage, because a preserved volume is
  refreshed in place and a stage-derived size then describes a file the
  volume does not hold — observed, and fixed by measuring the medium.
  `devdisk_test` mounts, grades the inventory, unmounts and mounts again: the
  second mount is the point, because a leaked write claim answers
  `AlreadyClaimed` forever.
- **`MAX_PROCESSES` is 1024.** One `just test` reached 256 exactly, which was
  the appliance's ceiling, and the quota gate's own note had already said the
  next utest needed the constant raised rather than the cap. A slot costs
  36 960 bytes of `.bss` for 768 of them — so what bounds it
  is neither memory nor `PROCESS_SLOT_BITS` but how many processes a `-j N`
  build wants at once.

**What this deliberately did not do.**

- **The build is not a gate and its output is not in the tree.** A cross LLVM
  plus a stage2 rustc is hours of CPU and tens of gigabytes; what CI grades is
  the plan, the wrapper and the two forks. `just toolchain` runs it, and the
  first number it produces is the one the `libLLVM` decision has been waiting
  for.
- **Nothing packages clang.** bootstrap's `dist` steps cover rustc, cargo,
  std and the llvm tools; `llvm.clang = true` only puts clang in the LLVM
  CMake build. Staging it onto the dev disk is a copy, and the copy is the
  script's, not bootstrap's.
- **The wrapper is not a driver.** It splits compile from link and routes
  arguments by shape; anything clang's real driver does that a shape cannot
  express, it does not do. It is written to be deleted, and
  `toolchains::SlopOS` is what deletes it.
- **An offline cargo is a smaller cargo.** No `publish`, `yank`, `owner`,
  `login`, `logout`, `search` or `info`; no git or remote-registry sources;
  `cargo new --vcs git` refuses and `cargo fix`'s dirty check sees no VCS,
  because without libgit2 there is no repository to see. `Cargo.lock` holds
  nine third-party crates and a vendored workspace reaches none of that.
- **No `cargo install`, no registry, no network.** The fork gives that up on
  purpose and the goal does not need it; a general dev machine does, and it
  is the same TLS-shaped work Workstream 1.2 names.

---

## Phase 1 — The toolchain

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

**What the reversal costs.** Five of the landed sections above: the dynamic
loader, which was always owed; the C++ runtime, which was new; the libc
surface underneath them, which was owed either way and which the LLVM decision
promoted from off the critical path to load-bearing; the port of
llvm-project itself, which was the phase's first real measurement and is the
one that came back cheapest; and the cross-build, which is the one that turned
out to be mostly configuration. The C99 frontend written
in Rust that the Rust-hosted road owed is **deleted** rather than deferred —
clang arrives in the same monorepo pass that produces `libLLVM.so` and
`rust-lld`, so the C compiler is a by-product of a decision taken for Rust's
sake, and that is the only place this road is cheaper than the one it replaced.

### Workstream 1.1 — The build loop holds (**M**)

A toolchain that starts is not a toolchain that finishes. What the loop needs
beyond the landed sections above, with the tree's current answer beside it:

- **Memory, and this is the one that moves.** rustc with LLVM peaks far above
  anything cranelift would have, and a build that overcommits currently dies at
  the faulting task with a SIGBUS-coded exit. Swap was an open decision under
  the Rust-hosted road; under this one it is a prerequisite of this phase, or
  a per-build memory budget is, and "decide later" stops being available.
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

### Workstream 1.2 — Getting code in and out (**S** for the goal, **M** beyond it)

Off the critical path, and this is a real scope reduction: `Cargo.lock` holds 47
entries of which only nine are third-party (`bitflags gimli libm limine paste
proc-macro2 quote syn unicode-ident unwinding`). Vendoring that is trivial, so
**building SlopOS on SlopOS needs no network at all** — no TLS, no crates.io, no
`git`. Those remain wanted for a general dev machine (there is no TLS anywhere:
`curl` rejects `https://` outright; DNS is one query at a time machine-wide; the
TCP window is capped at 32 KiB by a fixed buffer), but they are comfort beyond
the goal rather than a blocker for it.

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

Out of scope for the current goal, which ends at Phase 1 in QEMU. Recorded so
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

## Phase 4 — The toolchain rebuilds itself (not committed)

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
  with `libLLVM.so` in the single cross-build above. Having the compiler
  is not having the build system, the disk or the hours.
- **Disk and time.** A release LLVM build is tens of gigabytes of objects and
  hours of CPU on a machine with a real scheduler and real I/O. Neither number
  is worth estimating here; what is worth writing down is that both are an
  order of magnitude past Phase 1's, and that Phase 3's bare-metal list is
  where the I/O to support them would come from.
- **The `-Zbuild-std` dependency does not end.** Until
  `x86_64-unknown-slopos` is tier 2 and ships artifacts, every in-guest build
  builds std, which this phase inherits rather than fixes.

The honest framing: Phase 1 makes SlopOS a machine that develops SlopOS. This
phase makes SlopOS a machine that develops its own toolchain, which is a
different and much larger claim, and **nobody has made it.** Redox's January
2026 milestone is a natively *running* rustc and cargo that were cross-built on
Linux: its `mk/prefix.mk` `HOSTED_REDOX=1` branch `wget`s `rust.pkgar` and
`llvm21.pkgar` from its package server rather than building them, its LLVM
recipes hand CMake a host toolchain file for the native tablegen, and the
announcement's own list of what was built on Redox is "relibc, ripgrep,
cbindgen, and the Redox test suite" — no rustc, no LLVM, no GCC. Asterinas is
binary-compatible enough to run an unmodified NixOS userland and is still
*always* cross-built. So Phase 1's claim has been made once; this phase's has
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
      past* the cross-build above rather than a detour from it. Asterinas is
      also the proof of the ceiling: binary-compatible to the point of an
      unmodified NixOS userland, and still always cross-built. Binary
      compatibility buys running a prebuilt rustc; it does not buy a target
      that can be a host, and it does not remove the proc-macro `dlopen`.
- [ ] **Is `libLLVM` shared or static?** Shared is what upstream ships, what
      the 161 MB `librustc_driver.so` measurement was taken against, and what
      `bootstrap_slopos_toolchain.sh` writes today (`llvm.link-shared`);
      static removes one `dlopen` but not the proc-macro one, because
      `rustc_driver` is `crate-type = ["dylib"]` and no configuration changes
      that. It is no longer a question about what the loader owes but about
      how many `PT_LOAD`s and how much startup relocation an in-guest rustc
      pays for — a number neither cross-building the library nor planning the
      build produces, because both configurations compile and both plan. The
      first in-guest `rustc --version` is what answers it.
- [ ] **Does the toolchain's rustc unwind, or does the driver stop being a
      dylib?** The built-in target is `panic-strategy: abort` because every
      SlopOS binary is, and a rustc built that way aborts on its first fatal
      diagnostic: `FatalError::raise` is a `resume_unwind` and
      `catch_fatal_errors` a `catch_unwind`. Motor OS's fork answers only the
      second half — `rustc_driver`'s `crate-type` widened to
      `["dylib", "rlib"]` so a static compiler links — and keeps
      `PanicStrategy::Abort`, so no one's prior art covers the unwinding road.
      The cost of that road is `.eh_frame` in every userland binary and a
      `panic = unwind` std; the cost of the other is a compiler patch upstream
      will not take. Decide it with the first in-guest run, and do not answer
      it in the built-in spec alone: the JSON one is what the system's own
      binaries are built with.
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
      budget that makes overcommit not happen — before Workstream 1.1.

**Decided.** C++ runtime: **LLVM's `libc++`, cross-built, libc++ and
libc++abi linked into one `libc++.so`** — settled by building it, and by the
fact that `libstdc++` is not a library you cross-build but one a GCC
cross-compiler emits, which is a second toolchain to pin and keep. The libc
gap it needs is closed — `check_cxx_pin.sh` holds all 171 of its undefined
symbols to `libc.so` — and the LLVM build against it is measured:
`check_llvm_port.sh` compiles `LLVMSupport` for the target on every run. Syscall ABI: **Linux x86-64 numbering,
one table, a private
range at 1024, and a Linux number obliges the Linux signature.** Rust toolchain:
**LLVM, cross-built from Linux, with the C++ runtime ported to SlopOS** — the
Rust-hosted answer was decided first, then measured against this kernel and
found not to reach it. C is *not* excluded and is now cheaper, because clang
arrives in the same cross-build as `libLLVM.so`, which deletes the
Rust-written C frontend this plan used to owe. cargo: **a pinned fork that
puts curl, libgit2 and OpenSSL behind a `network` feature**, rather than
Redox's road of porting the five C libraries as recipes or Motor OS's of
shipping no cargo at all; SQLite stays, because three libc names were cheaper
than a build-system port. Scope: the full in-guest loop,
Phases 1–2, in QEMU. Identity: single-user, uid 0, permanently — so file
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
| Utilities | `userland/src/apps/coreutils/`, `userland/src/bin/coreutils.rs`, the justfile's `coreutils_tools`, `scripts/build_fs_image.sh`, `scripts/gen_initramfs.py` | *invariant* — the civil calendar is `slibc-core`'s and must not be re-inlined |
| Editor and toolkit | `editor-core/src/`, `userland/src/apps/editor/`, `appkit/src/`, `windowing/src/clipboard.rs`, the compositor's `protocol_pointer_grab` | *invariant* |
| Std/target/unwinding | `toolchain/{PIN,rust,libc}`, `scripts/make_slopos_sysroot.sh`, `scripts/lib/toolchain_pin.sh`, `scripts/check_toolchain_pin.sh`, `targets/x86_64-unknown-slopos.json`, `userland/userland.ld` | *invariant* |
| Compiler fork | `toolchain/compiler/`, `scripts/make_rustc_src.sh`, `scripts/check_rustc_target.sh`, `scripts/lib/toolchain_pin.sh`, `targets/x86_64-unknown-slopos.json`, `scripts/build_userland.sh` | *invariant* — the built-in spec and the JSON one are one spec in two files |
| Syscall ABI | `abi/src/syscall/numbers.rs`, `core/src/syscall/handlers.rs`, `scripts/check_syscall_abi.sh`, `scripts/gates/syscall/` | *invariant* |
| Backend and linker gates | `scripts/check_codegen_backend.sh`, `scripts/check_linker_script.sh`, `scripts/gates/{codegen,linker}/`, `targets/x86_64-slos.json`, `link.ld` | *invariant* |
| Storage | `fs/src/ext2/{dirindex,journal}.rs`, `fs/src/verity.rs`, `drivers/src/virtio_blk.rs`, `fs/src/fsreport.rs` | *invariant* |
| libc surface | `slibc-core/src/`, `slibc/src/{setjmp,locale,wchar}/`, `slibc/src/{stdlib/sort.rs,math/longdouble.rs,time/calendar.rs,string/convert.rs,stdio/{printf,scanf}.rs,conf.rs}`, `slibc/build/decls.rs`, `toolchain/libc/0001-slopos-libc.patch`, `userland/libctest/` | *invariant* |
| C++ runtime | `scripts/make_slopos_cxx.sh`, `scripts/check_cxx_pin.sh`, `toolchain/cxx/PIN`, `slibc/src/{unwind,cxa,math,ctype,stdlib,wchar,locale,setjmp,time}/`, `slibc-core/src/`, `slibc/build/`, `userland/cxxtest/` | *invariant* — `--print-abi-flags` is the one place the rune-table flag is written down |
| LLVM port | `toolchain/llvm/`, `toolchain/cxx/PIN`, `scripts/make_slopos_llvm_src.sh`, `scripts/check_llvm_port.sh`, `scripts/lib/toolchain_pin.sh`, `slibc/build/decls.rs`, `slibc/builtins/` | *invariant* — the patch is the port, and `libbuiltins.a` is where a shared object takes compiler-rt from |
| Cargo fork | `toolchain/cargo/`, `scripts/make_rustc_src.sh`, `scripts/check_cargo_fork.sh`, `scripts/lib/toolchain_pin.sh` | *invariant* — the patch shares the compiler fork's tree and stamp, and `rusqlite` stays only while slibc keeps `strspn`, `strcspn` and `FILENAME_MAX` |
| Cross-build | `scripts/bootstrap_slopos_toolchain.sh`, `scripts/check_bootstrap_config.sh`, `toolchain/compiler/000{2,3}-*.patch`, `toolchain/llvm/000{1,2}-*.patch`, `toolchain/llvm-rustc/`, `scripts/check_clang_driver.sh`, `Cargo.toml`'s `exclude` | *invariant* — the wrapper's two triples, and the workspace exclusion without which bootstrap does not build |
| Dev disk | `scripts/build_devdisk.sh`, `scripts/qemu_run.sh`, `userland/src/bin/tests/devdisk_test.rs`, `core/src/exec/grants.rs` | *invariant* — the marker's sizes are read off the volume, not off the stage |
| C++ platform | `vendor/unwinding`, `slibc/{staticlib,cdylib,crt0,include}/`, `NOTICE.md` | work |
| Phase 2 install | `scripts/qemu_run.sh`, `fs/src/devfs/mod.rs`, `fs/src/partition.rs` | work |
| Execution boundary | `AGENTS.md` | Phase 2 needs a scoped exception |

One thing is worth naming here because its paths sit apart from the section
that explains them: `libc.so` must keep exporting the seventeen `_Unwind_*`
entry points and must keep being built `-C force-unwind-tables`, because the
first frame of every unwind is one of its own.
