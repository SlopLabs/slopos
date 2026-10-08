# SlopOS Vulnerability Audit and CVSS Scoring

**No finding is open.**

Swept 2026-10-02: the installer and its medium — `/bin/installer` writing a
table, formats and a root onto a disk another system shares, the `install`
module served at `/media/install`, e2fsprogs run from the base with `Mount`,
the block ceiling the raw-device right lifts, slibc's additions for
e2fsprogs, `tree-core` installing over a root a guest wrote, and the GPT
writer and `fat-core`'s long names and format. Four reviews found defects in
the change before it landed, none in code that had shipped: a partition named
as the disk got a table written inside it; a disk with an MBR or an
unreadable GPT was erased without `--mode erase`; e2fsprogs held `Mount` by
identity, so any script the shell ran could format a disk (it now keeps
`Mount` only as far as a spawner that names it directly holds it); a link a
kept root held redirected the installer's manifest and seal writes off the
root, and a planted `e2fsck.conf` its log; and a reinstall would overwrite
another system's loader at `\EFI\BOOT\` on an ESP SlopOS had once made. Two
**pre-existing** defects, fixed here: `open(2)` dropped `O_NOFOLLOW` beside
`O_DIRECTORY`, so a directory walk that relies on the pair, Rust's
`remove_dir_all` among them, followed a link swapped in under it
(`test_open_directory_nofollow_refuses_a_link_to_a_directory`); and an ext4
block cache full of pinned entries answered as device damage and remounted
the volume read-only, which memory pressure alone could cause. Below the bar:
any process can see the new root's mount point while an install runs, and
the installer walks it without following links.

Swept 2026-10-01: the crash record — the panic path's polled writes to the
crash partition and the store's claim on it, `/dev/crash`'s listing, reads and
unlinks, `bootctl collect` writing what a record says into `/var/log/crash`
and `/var/lib/slopos/slots`, init running it with `Mount` and `Power` on every
boot, and the boot step that finds the partition by the GUID the loader
reports. A record is read or erased only with the raw-device right; the store
holds the partition's write claim, so no other writer reaches it and no table
re-read or whole-disk write moves the window the panic path writes, and every
write lands inside that window. Nothing a record holds names a file: the
collector takes the slot through `valid_slot` and the time as a number. Five
reviews found defects in the change before it landed, none in code that had
shipped: the collector could erase a record it had only copied to a RAM root;
a sequence number could repeat while the newest record was being erased,
leaving two records one name; one unreadable slot disabled the whole store;
the bare-metal hold could wait forever on a framebuffer lock a stopped CPU
held, so `panic=reboot` never reset; and an abort on an exhausted data stack
would have faulted again writing its record. Below the bar: any task can list
`/dev/crash` and so learn how many records it holds; a record carries kernel
addresses and the kernel log, which `/dev/kmsg` already gives every task.

Swept 2026-10-01: the boot chain — UEFI variables from user space and the
`BootEntry` capability, the boot manager variables' write checks, `bootctl`
and `install_test` reading every block node's GPT and writing the boot
partition, the kernel's GPT probe moved onto `boot-core`, load options and
device paths read off the firmware, and the host's disk builder. `Power`
reaches nothing it did not; `BootEntry`, which only `TASK_FLAG_INSTALL`
confers and only `install_test` holds, reaches `Boot####`, `BootOrder` and
`BootNext` and reads `BootCurrent`, and every write there is held to its
format on the kernel's own copy before the firmware sees it. Four reviews and
fuzzing of every `boot-core` parser found defects in the change before it
landed, none in code that had shipped: a new firmware entry could take a
number `BootOrder` still listed and boot first unasked; `bootctl`'s refusal to
overwrite the default slot failed open when `LoaderEntryDefault` named a menu
path rather than an entry; an entry stored with the disk's full device path
went unrecognised, so each install would add another; one unreadable firmware
entry aborted registration; and userland accepted a GPT entry overlapping an
earlier partition that the kernel skips. One **pre-existing** defect, fixed
here and below the bar, since only whoever writes a disk can craft it: the
kernel's probe accepted a header whose usable range reached LBA 0 or the other
copy's array, so a partition there could cover the table and a mount of it
overwrite it. Below the bar: `LoaderEntryDefault` is
machine-wide, so another loader that writes it moves SlopOS to slot a, or to
no install until `bootctl set-default`; a firmware entry outside `BootOrder`
past the first free number is not found.

Swept 2026-10-01: ext4 — `ext4-core`'s codecs, extent trees and jbd2
replay over crafted images, the kernel's journal, extents and checksums,
`FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` and the `Seal` capability, the mount
aliases, and the host's in-place conversion of a preserved root. Four
reviews found defects in the change before it landed, none in code that had
shipped: a `pwrite` at the last file block any process could make latched
the root read-only; an extent tree whose index entries share one node made
the mount's walk exponential; a commit block the medium damaged ended replay
silently, dropping the transactions committed after it; a checkpointed
journal could be reused before its emptied superblock was durable, which a
crash then replayed over newer homes; and the conversion removed a
`/.journal` the guest wrote on an ext4 root. One **pre-existing** defect,
below the bar: an operation that failed after unlinking the orphan list's
head left the head unrestored on the medium until the next commit, leaking
the inode until `e2fsck` if none came. Below the bar: the host's e2fsprogs
read and repair metadata the guest wrote, as they do any image they are
given, and a crafted root can at most fail its own build.

Swept 2026-10-01: the block layer and NVMe — raw reads, writes and flushes of
`/dev` block nodes, `BLKRRPART` and the size ioctls, the claims a mount and a
raw write take, `/dev/disk/by-*` and the `PARTUUID=`/`UUID=`/`LABEL=`
resolver, partition tables and filesystem superblocks read off removable
media, and an NVMe controller left running by firmware. A raw read, a raw
write and a re-read need the task to be privileged or hold `Mount`, a write
or a re-read is refused while anything holds the disk, and a claim is held to
the node its source resolved to. Four reviews found defects in the change
before it landed, none in code that had shipped: two partition entries could
share a window and each be mounted writable; a kill after the device held a
write left ext2 writable over a journal write of unknown fate; and the probe
let a controller master the bus before resetting it. Below the bar: any task
can read a disk's size, block size and volume labels and UUIDs, as Linux
exposes `/dev/disk/by-*` to every user.

Swept 2026-09-30: the tools the root now carries and what they reached —
`socket(2)`'s type flags and argument checks, `accept4(2)`, a new socket
description's blocking mode, libcurl on Mbed TLS, the recipe licence
closure, the default search path, and the host's installer of trees onto the
guest's disk (`scripts/fs_tree.py`, `build_fs_image.sh`'s floor and log,
`gen_verity.py`'s taint). One **pre-existing** defect, below the bar:
`socket` truncated its type argument to 16 bits, so `SOCK_CLOEXEC` was
dropped and a descriptor a C program asked to be close-on-exec reached every
program it executed. The process that created it chose what to run, and the
Rust standard library sets the flag with `fcntl`, so no privilege boundary
was crossed. Fixed with the flags, along with slibc's `accept4`, whose
`accept`-then-`fcntl` left the same window open under a concurrent `exec`.
The installer reads a disk a guest can write behind its filesystem's back,
since the guest builds and boots kernels of its own, so it puts nothing it
read from the image into a request, binds each directory listing to the
inode its parent names, and refuses a listing it cannot read; and the seal
measures what the guest wrote against the host's own record of what it
attested, never against the trailer on the disk. The holes four reviews found
in both were closed before they landed. Residual, below the bar:
whenever the guest leaves the root under its floor, the host runs `e2fsck
-fy` and `resize2fs` over metadata the guest wrote, as it already did for a
larger `PERSIST_IMAGE_SIZE`.

Fixed 2026-09-27, both **pre-existing**, reachable by any user and availability
defects at most, so neither is an entry. A dead task's post-switch teardown
runs preemptible in its successor, and a switch that landed inside it resumed
the successor there, where it tore down the next corpse on top of the first:
under a spawn-and-exit storm the teardowns nested until the 32 KiB kernel stack
overflowed into a double fault. A resume inside a teardown now queues its corpse
for the one already running (`test_corpse_is_queued_inside_a_running_cleanup`).
With that, `exit_stress_test` ran to its end and found the second: an account
release re-pointed a child's parent edge under a refund walk in flight, and the
child's pages reached the root both through that walk and inside the released
balance — an underflow that panics the tests kernel and silently skews the
ledger in a release one. A release now waits out the walks in flight.

Swept 2026-09-28: the POSIX surface ports expect and the memory policy
behind a cheap fork — `rt_sigqueueinfo`/`rt_tgsigqueueinfo` and their
forgery rule, the per-process shared signal set, `rt_sigtimedwait`,
signalfd, process-shared futex keys, `#!` dispatch and grant narrowing on
spawn, FIFOs, `fchdir`, lazy fork charging and the OOM killer, and the
recipe driver as supply chain. One guaranteed defect (confidence 83,
`AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H`, 5.5): the OOM killer weighed
processes by present pages and exempted only init, so an unprivileged
process could size unmapped memfds, fault a forked copy past the ceiling
and have the kernel SIGKILL the compositor, which `kill` refuses it. The
killer now weighs what each process's own account holds of the kind that
ran short and never takes a process the writer may not signal, except on
init's own write. Fixed inside the same unreleased change, so not an
entry. Two pre-existing holes surfaced with it and are closed: a process
nested past `MAX_ACCOUNT_DEPTH` got no ledger row and escaped every quota
and the commit ceiling, and a memfd whose sizer exited kept its frames
while its charge was credited out of every ancestor
(`test_oom_an_orphaned_memfd_stays_charged_to_nobody`). Below the bar and
fixed: a `kill` past a realtime queue delivered as `SI_KERNEL`, a
signalfd serving its creator rather than its reader, FIFO end counts that
saturated at 65 535, and recipe fetches that followed non-https
redirects. Process-shared futex waiters on a file any process can map are
reachable by every process, as on Linux; SlopOS has no file permissions to
narrow it.

Swept 2026-09-16: the editor — `editor-core`'s buffer, lexer, search and tree
model, the new `appkit` surfaces, the fd-based clipboard transfer in
`windowing`, and the editor's own filesystem boundary, every one of which reads
input the user did not write. Nothing reached the confidence bar. The editor
holds the user's own authority, so a file it opens or writes crosses no
boundary; what the sweep found instead were robustness and data-integrity gaps,
all closed inside the same unreleased change and so never entries.

Swept 2026-09-14: the utilities becoming executables — a multicall binary whose
archive extractor, patch applier, regex engine, inflater and digest all read
attacker-supplied input, plus the `/bin` symlink install and the `execve`
thread-pointer reset. Three guaranteed defects and six below the confidence bar,
all fixed inside the same unreleased change, so none is an entry.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no
> backwards-compatibility or audit-trail obligations, so this file tracks **open
> findings only**. A resolved finding is **removed**, never kept as a `fixed`
> record, and a defect fixed inside the change that introduced it never becomes
> one. IDs stay stable while a finding is open and are never reused, so gaps are
> expected. The git history is the audit trail: `git log -p -- CVSS.md` recovers
> any entry that was here, and a fix's own test is the durable record of it.

Swept 2026-09-19: the dynamic loader — the kernel's `PT_INTERP` path and the
userland linker behind it, both of which parse attacker-chosen ELF. Twenty-two
defects across three reviewers, every one fixed inside the same unreleased
change and so not an entry. The sweep also proved a **pre-existing** defect the
interpreter work made visible by contrast, which is the entry below: the
executable's own segments have never been covered by a VMA.

Swept 2026-09-19: the C++ runtime — the cross-built `libc++`/`libc++abi`, the
libc surface it needed (`<math.h>`, `<ctype.h>`, the `strto*` family,
`pthread_once`, the `__cxa_*` trio), the Level-1 unwinder now in every image's
`libc.so`, and the `.init_array` walk a static program needs. Three reviewers.
One memory-safety defect, found by the third: `atexit(3)` registers a null
`__dso_handle` — slibc has one shared `atexit` where glibc links a per-object
copy from `libc_nonshared.a` — so `finalize_range` never reclaimed an `atexit`
made from inside a `dlopen`ed object, and `exit` then called it through an
unmapped address. It is also a slot leak: a `dlopen`/`dlclose` loop consumes
the 256-entry table permanently. Fixed inside this change by testing the
handler and argument addresses against the unloaded span as well as the
handle, so not an entry. No privilege boundary is crossed either way — a
process can only do this to itself, and `dlopen` already runs code of the
caller's choosing. The rest of what the reviewers found was correctness
(`std::stod("0e1")` throwing on a spurious `ERANGE`, a missing sentinel guard,
a nonsense load-bias fallback), all closed in the same unreleased change.

Swept 2026-09-21: the commit ledger and the build-loop plumbing — every
charge point (`mmap`, `brk`, `mprotect`, `fork`, `exec`, `memfd` sizing, the
demand and stack-growth faults), every refund road including the unmap error
arms, `F_DUPFD_CLOEXEC`, the pipe `fstat`, the file map's admission change,
slibc's `posix_spawn` over the spawn primitive with its parent-side descriptor
plan, and the process exit path. Three reviewers, twenty-odd findings, all
closed inside the same unreleased change and so none an entry: an `exec`
refused after the point of no return (now sized and charged beside the old
image), `MAP_NORESERVE` lost across a `PROT_NONE` reservation, refunds missing
on the `munmap` and `MAP_FIXED` error roads and on a failed stack reset, and a
`posix_spawn` that answered `E2BIG` where it should have fallen back to
`fork`. The sweep also proved a **pre-existing** defect the ledger made
visible by measuring it: since the task-to-task switch, a process that ended
itself never had its address space destroyed or its registry entry retired —
the first exit-cleanup pass consumed the "last task left" latch the second
pass needed — so every self-exiting process leaked its frames, page tables and
one of the 1024 process slots until shutdown. Any user reaches the exhaustion
by exiting 1024 processes, but no boundary is crossed and nothing is read or
written that should not be, so it would have been an availability entry at
most; it is fixed here (`TASK_EXIT_LAST_IN_PROCESS`) with the build-loop
test's "commit comes back when a process exits" as its durable record, and is
therefore not an entry. Fixing it reached a second pre-existing defect:
an account row named its parent by a bare arena slot, and a released slot is
reissued under a new generation at once, so a process whose parent exited and
was replaced before its own deferred teardown ran credited its outstanding
charges to the stranger now in that slot — a row underflow that panics the
tests kernel and silently corrupts the ledger in a release one. Any user
reaches it with a fork-and-exit ordering (`exit_stress_test` finds it in
seconds), so it is an availability defect at most; the parent edge carries
the parent's generation now and a released row hands its children to the
grandparent, so it is fixed here and not an entry. The stress test then
reached a third, in the tests kernel only: the per-CPU klog capture ring took
a bare spin flag with interrupts on, so a writer switched out mid-append left
its CPU's next writer spinning forever, and a spinner holding an
interrupt-masking lock stopped acking TLB shootdowns and wedged every CPU
behind it. The shipped kernel registers no capture backend and so never
takes that lock; the ring masks interrupts while held, acks shootdowns while
it waits, and drops an append nested from an NMI, so it is fixed here and
not an entry. Run on a host without KVM it then found two waits whose only
releaser was the waiting CPU, both pre-existing: a dispatcher that dequeued
the current task after a raced wake spun on that task's own `on_cpu` flag
with every other CPU idle, and a user copy switched out mid-copy pinned a
reference to the address space that an exclusive syscall on a sibling
thread spun for under the process-VM lock, while dispatching the copier
took that same lock; the spin's budget broke the cycle by failing the
syscall, so a four-thread process lost a thread's guard-page `mprotect` as
`EPERM`. Any user reaches both with threads that block and wake under
load, an availability defect at most; the claim hands the current task
back and the copy holds off preemption while it holds the reference, so
they are fixed here and not entries.

Swept 2026-09-23: the network path a self-hosted build fetches through — the
TCP stack's chunked rings, window scaling, persist, retransmission and
keepalive timers and the way a connection's end reaches its socket; the
concurrent resolver; `tls-core`, whose record layer, handshake and certificate
path read what a server chooses; `curl`, `nc` and `getaddrinfo`; and the dev
disk's export, which reads a volume the guest wrote. Five review passes, each
by fresh reviewers. What the change introduced was closed inside it, so none
is an entry, and it fixed one **pre-existing** defect that would have reached
the bar. The resolver's cache was keyed on a 32-bit FNV-1a hash of the name
and compared nothing else, so two names whose hashes collide answered each
other — `glbvs.example` and `yacxa.example` do — and anyone who could make the
machine resolve a name of their choosing (a local process, or a server whose
redirect `curl` follows) could plant an address under any other name for its
TTL, the collision found offline in seconds. Plaintext protocols follow a
planted address; TLS refuses it at the certificate. The cache now compares
whole names, with `dns_tests`' collision case as the record, and a reply must
also echo its question where the ID and port alone were taken before. The rest
were availability defects, most of them a peer's to trigger: a `DataState`
allocation `.expect` that a handshake's last segment under memory pressure
turned into a kernel panic, a FIN sent ahead of queued bytes that the send
map's accounting caught as a panic, a late retransmission timer that left two
running, a shut window nothing probed, a lost FIN never resent, lost bytes
that only a timeout would resend, a retransmission overlapping bytes already
taken dropped whole, FIN_WAIT_1 held for good by a peer that only sent data,
and TIME_WAIT, LAST_ACK and keepalive each ending connections a reader or an
answering peer still needed; each has a test in `slopos_net::tests`. Below the
bar (confidence about 60): the CSPRNG is seeded from four TSC reads on a CPU
without RDRAND, so a ClientHello's random lets an observer
search the seed and with it the key share. Every x86-64 CPU of the last decade
and QEMU's `-cpu max` have RDRAND, and the plan records it as a limit.

Swept 2026-09-24: the toolchain running in the guest and building the kernel
— the user-mode trap stack, `wait4`'s usage report, the executable's segment
VMAs, zero-length user ranges, user copies that read a file-backed page in,
the buddy allocator's free lists, and the KTAP subtest path. Three review
passes, each by a fresh reviewer; the third found nothing above a nit.
What the change introduced was closed inside it, and it fixed four
**pre-existing** defects. A trap from user mode pushed from `TSS.RSP0`, which
sat a fixed 12 KiB above the frames of the round trip that entered user mode,
so a chain deeper than that wrote over those frames and the kernel took a
general protection fault returning through them; `devdisk_test` reached it as
it started, on the unoptimized tests kernel. That is kernel stack corruption
from user mode, fixed here by
starting every trap at the round trip's own RSP, with
`test_deep_user_trap_spares_the_round_trip` as the record. SLOPOS-2026-0056,
the executable's segments mapped with no VMA, is fixed by giving them the
per-segment VMAs the interpreter already had
(`test_a_large_image_owns_its_segments_and_the_heap_clears_it`); its adjacent
half, the paths that drop `NO_EXECUTE`, is carried forward as
SLOPOS-2026-0057. The buddy allocator walked a free list to take a buddy off
it, so freeing a large address space held its interrupt-masking lock for a
time proportional to the list and stalled every CPU behind it; any process
reaches it by exiting after a large allocation, an availability defect at
most, and the lists are doubly linked now. The worst was `fork`: the clone
recorded the parent's frames under the parent's lock but took the child's
references to them only after dropping it, so a sibling thread's `munmap` in
that window freed a frame the child then mapped — and, reallocated in time, a
frame of another process. Any multithreaded process that forks while another
of its threads unmaps reaches it; cargo does on every build, which is how the
guest found it, where the frame's metadata had already been released and the
fork failed. A read or write of another process's memory would have put it
above the bar; the snapshot now holds a reference on every frame it records
(`test_cow_clone_survives_a_sibling_unmap`), so it is fixed here and not an
entry.

Swept 2026-09-26: copy-on-write and the file-backed fault path, which the
guest's kernel build changed to map a page set's frame into a `MAP_PRIVATE`
mapping until the first store. The change leans on the COW marker, and reading
what the marker was trusted with found two **pre-existing** defects, fixed here
with a test each rather than entered. `mprotect` wrote `WRITABLE` onto every
present leaf, COW-marked or not, so a process that forked and then re-applied
`PROT_READ | PROT_WRITE` to its own range stored straight into frames its child
still mapped — one process writing another's memory, confidence 90, which
would have scored above the bar
(`test_mprotect_keeps_a_forked_page_copy_on_write`); the fork also left the
parent's read-only private pages unmarked, which the same `mprotect` reached.
And a write fault on a COW leaf was resolved without asking the region, so a
forked child stored to its `PROT_READ` pages
(`test_cow_write_to_a_read_only_region_is_fatal`). SLOPOS-2026-0057 is fixed
with them: the COW copy, the ring and the shared `memfd` mapping now take their
leaf flags from the region they map (`test_cow_copy_keeps_the_region_no_execute`).

Swept 2026-09-26, second pass: the install path — raw writes to
`/dev/vd*` nodes, UEFI variables from user space, `bootctl`, and `fat-core`
parsing an ESP it did not write. Two reviewers. Raw writes are held to a
`Mount` or `SYSTEM` holder and to the device's exclusive claim, so a mounted
device refuses them. The one widening found was closed before it landed:
`efivar_set` passed any vendor GUID through, which would have let a `Power`
holder — a capability that exists to reboot — rewrite `BootOrder` or enrol
Secure Boot keys; it now takes only the Boot Loader Interface's GUID and
SlopOS's own. Also closed in the change: an answer a killed caller left on
the firmware thread could be handed to the next caller. `fat-core` bounds
every chain walk, so a hostile ESP yields an error rather than a hang.

Swept 2026-10-03: wired networking — the RTL8168 receive ring, which reads
lengths and status a device writes; `nic::publish`, ARP and IPv4 ingress, now
facing a physical LAN; slibc's `res_query` and `dn_expand`, which parse replies
from the network; and the host's `sshd -i` fixture the guest reaches at
10.0.2.4:22. Three review passes. Nothing reached the bar, and what came
closest was fixed in the change rather than entered. The driver had opened the
chip's receive filter to 16 KiB over 2 KiB buffers, the precondition of
CVE-2009-1389 (confidence 55); RxMaxSize is now the length a descriptor
names. IPv4 ingress delivered 127/8 and foreign destinations arriving on a
physical NIC, so a LAN host reached a socket bound to 127.0.0.1 (confidence
75); `ipv4::admits` now drops them, admitting only DHCP's unfragmented
datagrams to port 68 on a device that holds no address yet. A connected UDP
socket's `poll` counted datagrams its `recv` discarded, which could hang
`res_query` (confidence 45), and `res_query` kept an id a failed `getrandom`
left zero and asked from the allocator's next port (confidence 30); both are
fixed. The fixture's sshd takes one client key, forces a command that serves
the checkout read-only and the run's scratch repository, and forwards
nothing. Pre-existing and left as designed: a UDP bind with `SO_REUSEADDR`
takes over an identical binding whatever its holder set
(`test_so_reuseaddr`).

Swept 2026-10-07: USB host controllers — `usb-core`'s register,
extended-capability, TRB and ring codecs and the handoff, reset, configure,
run and drain sequences, over registers and event TRBs a controller writes;
the `xhci` probe taking each controller from the firmware's SMM; the pages it
hands the controller, across a failed probe, a dead controller and shutdown;
the `usb` thread and the interrupt drain; `set_power_d0`'s BAR restore and
BAR sizing with decode off; the `usb=` knob; and the host's QMP socket. Three
review passes, a security pass and a fuzz run. Nothing reached the bar, and
nothing in the change is reachable from user space: no device node, ioctl,
syscall or kconsole command. Every register set the capability registers
place is held inside BAR0 before it is touched, the extended-capability walk
is bounded and checked entry by entry, a Port Status Change names a port
only within MaxPorts, and a completion retires only a TRB still in flight; a
million random and shaped register files and event pages, driven through
every sequence over a bus that panics as `IoMem` does, panicked nowhere. A
page goes back to the allocator only once the controller is off the bus, and
a dead or shut-down one keeps its pages. What came closest was fixed in the
change: probe mapped BAR0 before checking for room, so a device with many
xHCI functions could spend the dynamic MMIO ranges a later driver, the root
disk's included, needed (confidence 45); it now declines without touching
the function unless a range for BAR0, and two for an MSI-X table when it has
no MSI, are free, though one declined on its capabilities keeps its BAR0
range. Below the bar and left: an unresponsive controller holds boot for up
to about a minute (35); a device that toggles its connection floods
the kernel log (30); and the test's QMP socket gives the user's QEMU to
whoever can connect to it, which the umask limits to the user (30). By
design, and recorded in `plans/usb-xhci.md`: taking a controller ends the
firmware's emulation of a USB keyboard. Pre-existing and left: `QEMU_DEBUG=1`
puts its monitor socket in `/tmp`, where another user can create the path
first and keep QEMU from starting.

Swept 2026-10-07: USB enumeration and the device model — `usb-core`'s
standard requests, descriptor walker, hub descriptors and status, transfer
rings and the enumeration tree, over every byte a device, a hub or the
controller sends; the kernel's per-device slot memory, rings, store and
pipes; `UsbBus`, its claim release and the bind thread; removal across a
controller that dies; the `usb settle` boot step; the kconsole listing; and
the host's QMP-driven test. Six review passes and a fuzz run of 30 000
enumerations of mutated devices behind mutated hubs, each pulled after a
random time, which panicked nowhere and left no slot. Nothing reached the
bar, and nothing in the change is reachable from user space: no device node,
ioctl or syscall, and the listing is informational. Everything a device
controls needs physical access to a port. What came closest was fixed in the
change. A hub could keep its ports, and with them the controller's one
default-state turn, frozen by failing requests in the right order, or loop
forever on a change bit it stalled every clear of, and in a dev or tests
kernel overflow its error counter into a panic after about an hour of
failures (confidence 70); a hub that fails eight requests between two port
statuses is now removed and its port given its three tries. A driver's
abandoned transfer stayed on the ring, so the next write could send its
bytes twice or a read take another's (55); the ring now stops until the
USB thread moves it past. A command that never completed held every later
one and left removal waiting forever (50); it now kills the controller. A
4 KiB configuration was walked quadratically with interrupts masked (40), a
port whose change bits never cleared could hold the USB thread in one pass
(40), and a configuration value of 0 unconfigured the device it was set on
(30); each is fixed.

Three more were found in endpoint-halt recovery, and are fixed. A device
that stalled every `CLEAR_FEATURE(ENDPOINT_HALT)` kept its endpoint in a
clear-and-recover loop forever (45). A Set TR Dequeue Pointer refused after
Reset Endpoint reopened the ring while the device's endpoint was still
halted and the two data toggles disagreed (40). An endpoint left halted
kept EP0 from ever being recovered again (40).

Below the bar and left: a device that toggles its connection still floods
the kernel log (30), as Phase 1 recorded, and the laptop half — a low-speed
device, a high-speed hub's transaction translator, a USB 3 hub — is graded
only by the simulated controller until the laptop run.

The highest ID issued so far is **SLOPOS-2026-0057**. The next finding is
`SLOPOS-2026-0058`.

## Open findings

None.

## Cadence

1. Sweep after each major milestone and before any release or PR handoff.
2. Re-scan what recent commits touched — at minimum syscall paths, memory
   management, filesystems and drivers.

## Triage workflow (strict order)

1. **List every finding first**, unscored.
2. Score confidence 0-100: evidence quality (0-40, direct code with exact
   `path:line`), exploitability clarity (0-30, a realistic attacker path),
   reproducibility (0-30).
3. Only **confidence >= 80** is a guaranteed issue.
4. Only a guaranteed issue gets a CVSS vector, computed with
   `python3 scripts/cvss_calc.py "CVSS:3.1/..."` so agents agree.
5. A finding below 80 with an attacker-reachable trigger is still recorded, with
   no vector and a note saying what evidence would raise it. One with no
   attacker-reachable trigger belongs in `plans/`, not here.

## Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- **Verify the claim before fixing it.** An entry can be wrong; read the code.
- A fix lands with a test that **fails without it** — confirmed by reverting the
  fix, not by assertion.

## Entry format

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps; if none is
  safely possible, say why and give the nearest deterministic validation
- Remediation: the shape of the fix, not "add a check"
```

## Scoring notes

CVSS 3.1 Base Score; `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`,
`9.0-10.0 Critical`.

SlopOS has no credential model, so "unprivileged local attacker" means any
process that can execute code: `AV:L/PR:L`.

A panic in a `#![forbid(unsafe_code)]` crate is an availability impact, never a
memory-safety one. Overflow checks are on in the dev and tests kernels and off
in release, so the same arithmetic defect is a panic in one build and a silent
wrong value in another — say which when scoring.
