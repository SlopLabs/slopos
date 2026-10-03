# SlopOS On Bare Metal

## Goal

Run the loop of `plans/self-hosting.md` on a real machine. Install SlopOS from
a USB stick onto a disk it shares with another OS, boot it from that disk,
build and install the system there, and exchange commits over the wired
network. The first machine is the Lenovo laptop the live ISO already runs on.
Other machines' platform support is not this plan's.

| Part | What it is | Where |
|---|---|---|
| CPU | Raptor Lake-P, 12 hardware threads | — |
| Display | Intel UHD Graphics `8086:a7a8` | `00:02.0` |
| Disk | Kingston NV3, DRAM-less NVMe `2646:5027` | `01:00.0`, on the CPU's PCIe 4.0 port; no VMD |
| Wired NIC | Realtek RTL8111/8168 `10ec:8168`, rev 15 | `02:00.0`, on a PCH root port |
| Wi-Fi | Intel CNVi `8086:51f1` | `00:14.3`; out of scope |
| USB | PCH xHCI `8086:51ed`, Thunderbolt 4 xHCI `8086:a71e` | `00:14.0`, `00:0d.0` |
| SATA | AHCI `8086:51d3`, no disk attached | `00:17.0` |

Secure Boot is off. CachyOS boots through GRUB 2.13 from the disk's ESP, and
its firmware entry, `cachyos`, is the only one.

## Where it stands

`just iso` builds a hybrid ISO that boots the laptop from RAM:

- Limine loads the kernel and the base.
- The Xe driver draws the desktop.
- The i8042 keyboard and the I²C-HID touchpad are found through the ACPI
  namespace.
- The kernel log and a panic render on screen.
- UEFI resets and powers the machine off.
- The kernel carries an NVMe driver, graded on QEMU's model, which lists a
  disk's partitions under `/dev/disk/by-partuuid`. The live system boots
  `root=initramfs` and mounts no disk.
- Every volume is ext4 in one profile, with a jbd2 journal that e2fsck
  replays.
- The boot chain shares a disk: Limine under `\EFI\SlopOS\`, the slots on a
  boot partition of their own, slot selection through `LoaderEntryDefault`,
  and a firmware entry only the installer's role may register. The QEMU A/B
  loop runs on that layout.
- A fatal panic leaves its report and the kernel log's tail in the boot
  disk's crash partition, and the next boot moves it to `/var/log/crash/`.
  Under `panic=reboot` on bare metal the panic stays on screen for ten seconds
  before the reset.
- The ISO is an install medium: `/bin/installer` lays SlopOS out on a disk,
  beside another system or over it, from what the loader brought, and every
  base carries e2fsprogs.
- The kernel carries a driver for the laptop's RTL8168h, graded by host tests
  over a simulated chip. Every NIC takes a DHCP lease, and git reaches another
  machine over SSH.

## Phases

There are seven phases. Each ends in something that runs, and each lands as
commits of its own.

| Phase | Needs | Ends with |
|---|---|---|
| 1. NVMe disks — **done** | — | the dev loop on NVMe; the live ISO sees the laptop's disk |
| 2. ext4 — **done** | — | every image the tree builds is ext4 |
| 3. A boot chain that shares a disk — **done** | 1 | the A/B loop on the new partition layout |
| 4. A crash record — **done** | 1, 3 | a slot that panics leaves the panic behind |
| 5. Installer and install medium — **done** | 1, 2, 3 | **milestone 1:** SlopOS installed beside CachyOS, self-hosting offline |
| 6. Wired network — **done** | — | **milestone 2:** git and crates.io over the RJ45 port |
| 7. Full speed | 5 | a native build measured, and made faster if the CPU clock is the cause |

Phases 2 and 6 can run side by side. Phase 4 comes before phase 5 because the
first installed boots on the laptop are where a crash record pays off.

### Phase 1: NVMe disks — done

Built: the block layer (`drivers/src/block`) every block driver registers
with, the NVMe driver (`drivers/src/nvme`, its layouts in `nvme-core`), and
the QEMU disks moved onto `-device nvme`. `AGENTS.md` describes both; the
decisions later phases build on are under Decided.

Left for the laptop, which only the user's run grades: the NV3's partitions
listed under `/dev/disk/by-partuuid` from the live ISO, and the host memory
buffer it asks for granted.

### Phase 2: ext4 — done

Built: `ext4-core`, the format as host-tested data — checksums, codecs, the
extent tree, directory tails, jbd2 and its recovery — graded against images
e2fsprogs made; the kernel's driver on it, writing the profile in
`ext4-core/profile` and reading ext2 and ext3; the journal as jbd2 in the
journal inode, keeping the ring, grouped commits, ordered data and bounded
writeback; `FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS` with the `Seal`
capability; every host image formatted in the profile, the persistent root
converted in place; and `just test-rude-exit`, in which the host's e2fsck
replays what a dying boot committed. `AGENTS.md` describes all of it; the
decisions later phases build on are under Decided.

Left for the developer's machine: its own `ext2-persist.img`, which converts
on the next `just boot`.

### Phase 3: A boot chain that shares a disk — done

Built: the layout every SlopOS disk has, stated once in `boot-core` and laid
out by `scripts/build_bootdisk.sh` through `tools/bootdisk`:

| Partition | Filesystem and type | Contents | Written by |
|---|---|---|---|
| ESP | the disk's existing ESP, or a new 260 MiB one | `\EFI\SlopOS\BOOTX64.EFI` (Limine 12.9.1) and `\EFI\SlopOS\limine.conf` | installer |
| SlopOS boot | FAT32, SlopOS type GUID, 1 GiB | `/boot/{a,b}/kernel.elf` and `base.img` | `bootctl` |
| SlopOS root | ext4, SlopOS type GUID | `/` | the system |
| SlopOS crash | raw, SlopOS type GUID, 4 MiB | the last panic | the panic path |

`boot-core` also carries the GPT reader the kernel's partition probe uses,
load options and device paths, the boot manager variables' formats, the Boot
Loader Interface's strings and the Limine configuration renderer, with an
`efi_boot_entry` per other system. `bootctl` finds the boot partition through
`LoaderDevicePartUUID` and commits through `LoaderEntryDefault`; it never writes
the ESP. The `BootEntry` capability, conferred by the installer's role
`TASK_FLAG_INSTALL`, reaches the boot manager's variables, and the kernel holds
every write there to its format. `userland::boot_disk` registers the firmware
entry. `just test-install` runs on one disk in the bare-metal shape: it boots
through the registered entry and holds the ESP to the bytes it was built with.
`AGENTS.md` describes all of it; the decisions later phases build on are under
Decided.

### Phase 4: A crash record — done

Built: `boot-core::crash`, the crash partition's format as host-tested data —
64 KiB slots, a header sealing each record's text with a CRC-32, a summary of
`key: value` lines, and which slot the next record takes; the kernel's crash
store (`drivers/src/crash.rs`), which finds the partition at boot on the disk
whose GPT disk GUID Limine reports for the kernel, holds it under a write
claim, and on a fatal panic writes the report and the kernel log's tail
through the NVMe panic queue before any reset; `/dev/crash`, a file per record
that reads its text and erases it on unlink; `bootctl collect`, which init runs
first on every boot, and `bootctl status`, which reports each slot's last boot;
and `panic=reboot`'s ten-second hold on bare metal. `just test-install` grades
it: the slot that panics leaves a record the fallback boot moves to
`/var/log/crash/`. `AGENTS.md` describes all of it; the decisions later phases
build on are under Decided.

Left for the laptop, which only the user's run grades: a panic written through
the NV3's panic queue, and the hold on its screen.

### Phase 5: Installer and install medium (milestone 1) — done

Built: the install medium, an `install` module the live ISO carries
(`scripts/build_install_medium.sh`) — Limine for the ESP, the source of the
base's recipe programs, and with `PAYLOAD=1` the toolchain and a `--vendored`
clone of `HEAD` — which the kernel serves at `/media/install` beside the
kernel and base Limine booted, a second `basefs` instance; e2fsprogs as an
unpatched recipe, its six programs in every base at `/sbin`, and the slibc it
needed; `/bin/installer`, every answer also a flag, with the GPT writer and
the placement plan in `boot-core`, long names in `fat-core` and the host-tree
rule in `tree-core`; and `just test-installer`, which installs from a USB
stick onto a blank disk, beside another system and over an existing
partition, takes each disk once around `selfhost.sh install` with the stick
gone, and reinstalls over the blank one keeping its root. `AGENTS.md`
describes all of it; the decisions later phases build on are under Decided.

Left for the laptop, which only the user's run grades: SlopOS installed beside
CachyOS from the stick (`PAYLOAD=1 just iso`), booted from the NV3, and
`selfhost.sh install`, `bootctl reboot` and `bootctl commit` run there with
no network.

Left open: a clean file page a process has mapped is pinned until unmapped,
so the file map's per-process share, an eighth of memory, bounds what one
link may map; the debug tests kernel's link outgrew 4 GiB's, and the
self-hosting checks boot at 6 GiB. Reclaiming such pages under pressure is
what lets that bound go.

### Phase 6: Wired network (milestone 2) — done

Built: the network stack's own netpoll and net-timer threads and
`nic::publish`, which brings any NIC into service with a DHCP client; egress
frames, ARP and neighbour traffic that carry the identity of the device they
leave on, the hard-coded `DevIndex(1)` gone; ingress on a physical NIC held to
what is addressed to the host; `rtl8168-core`, the RTL8168h as host-tested data
and register sequence over a simulated chip, and its kernel driver
(`drivers/src/rtl8168.rs`); OpenSSH 10.5p1 as a recipe, whose `ssh` and
`ssh-keygen` git and the ladder run, and the slibc it needed; and a ladder
rung in which the guest's git fetches and pushes over SSH through the host's
own `sshd`. `AGENTS.md` describes all of it; the decisions later phases build on
are under Decided.

Left for the laptop, which only the user's run grades: the RTL8168h brought up
without the PHY patch firmware, a DHCP lease on the RJ45 port, `git pull` and
`git push` reaching the development machine over SSH, and cargo fetching from
crates.io.

### Phase 7: Full speed

- **Frequency.** The tree has no HWP or P-state code. Measure the effective
  frequency (APERF/MPERF) during a guest build, and enable HWP if it runs
  below turbo.
- **Hybrid cores.** The scheduler does not tell P-cores from E-cores. Measure
  the cost before changing that.

**Done when** a guest build's effective frequency on the laptop is measured,
and, if HWP is enabled, the same build is timed before and after.

## Grading

Everything but the Realtek driver's hardware half and the NVMe host memory
buffer is graded in QEMU. Agents never run on hardware (`AGENTS.md`); the
laptop run is the user's acceptance, and the only grade those two get.

**`just test-installer`** uses the pinned NV-varstore OVMF, one NVMe disk, and
the ISO attached as USB storage on `qemu-xhci`. The firmware reads the ISO and
the kernel does not, just as with a stick. Three disks are graded:

- **A blank disk,** installed with erase-disk.
- **A disk holding a foreign OS,** installed into free space. The disk has an
  ESP with `\EFI\other\`, one foreign data partition and a foreign `Boot####`
  entry, which the live system registers before it installs. Afterwards the
  foreign partition and the foreign ESP files are byte-identical, the foreign
  firmware entry is still in `BootOrder` and Limine's menu offers it. The
  foreign entry names a partition on the disk QEMU boots by `bootindex`: OVMF
  deletes an `HD()` entry it cannot match to such a device.
- **An existing partition,** reused as the root.

Each run then boots from the disk with the ISO detached, a second QEMU on the
varstore the first left, and goes around `selfhost.sh install` once. The host
holds the root to `e2fsck -fn`. `INSTALLER_PAYLOAD=0` installs without the
toolchain and clones a slot where it would build one.

**Crash record.** In `just test-install`, a slot booted with `panic.boot=on`
falls back to the committed slot, and the fallback boot finds the panic in
`/var/log/crash/`.

**Filesystem.** `just check-fs-image` and `just test-rude-exit` run in CI.

## Out of scope

- **Wi-Fi.**
- **USB.** See `plans/usb-xhci.md`. It is what later lets the live system read
  the medium from the stick instead of from RAM, and what brings USB NICs.
- **Other machines' platforms:** AHCI (the laptop's SATA controller has no
  disk), VMD, more than 17 CPUs, timers without HPET, x2APIC mode, INTx, PCI
  without MCFG, other PCH GPIO blocks.
- **ACPI events and suspend.**
- **Secure Boot signing.** The design allows it: commits do not touch
  `limine.conf`. With a config hash enrolled, Limine wants a BLAKE2B hash on
  every path, so installing into a slot would rewrite the configuration and
  re-enrol it. The signing itself is not planned.
- **Resizing another OS's filesystem.**
- **Other filesystems.** Mounting, or installing onto, btrfs (CachyOS's
  default), XFS or any other filesystem is future work, not excluded. This
  plan picks the filesystem SlopOS formats its own root with. Any other
  filesystem is written from its format documentation, as ext4 is here, since
  no GPL-2.0-only or CDDL code may be taken.
- **A package repository.**

## Decided

- **One block layer, one request engine.** Every block driver registers its
  disks with `drivers/src/block` and supplies only a `QueueOps` transport; the
  request slots, their bounce pages, the timeout quarantine and the
  abandoned-write fence are shared, and so are their tests. A USB
  mass-storage or AHCI driver is a transport.
- **Names:** `nvme<C>n<N>` numbers controllers in probe order and namespaces
  by NSID, not Linux's per-subsystem head instance: stable, and the same as
  Linux's on every drive with dense NSIDs.
- **Claims cover what they name.** A partition's claim excludes the whole disk
  and itself, so a disk's partitions mount side by side; a write claim is
  exclusive and read claims, which read-only mounts take, share; a table
  re-read needs nothing on the disk claimed. The installer writes a table
  through a whole-disk claim, drops it, and re-reads.
- **A filesystem block is at least a logical block.** A partial block is a
  read-modify-write inside one request slot, which a torn write turns into
  damage outside the transaction, so the driver refuses a volume whose blocks
  are smaller than the device's. The profile's 4 KiB blocks cover the
  512-byte and 4 KiB logical blocks drives report.
- **The panic path owns a queue pair.** It is created at probe, polled, holds
  64 KiB, and is taken with a `try_lock` that never waits; phase 4 builds on
  `PanicQueue` and nothing else.
- **Devices are told the power is going** through `driver_core::shutdown`,
  after the filesystems write back, on poweroff and reboot. A panic reset does
  not run it.
- **The host memory buffer** is granted up to 128 MiB in chunks of at most
  4 MiB, after the namespaces register, and taken back before the shutdown
  notification.
- **A request the device never answers stays quarantined.** There is no
  Abort and no controller reset: a controller that drops ten I/O commands
  serves nothing more until a reboot, and one admin command it never answers
  holds back every later one, the host memory reclaim and the shutdown
  notification among them. Recovery waits for a drive seen to need it.

- **ext4 first.** It is the filesystem SlopOS formats its root with, and the
  first one it supports beyond ext2, sharing ext2's on-disk core: one driver,
  cache and journal serve both. Its on-disk format is
  documented, e2fsprogs is its reference toolset, and a Linux on the same disk
  can check and replay a SlopOS root. btrfs's snapshots, data checksums and
  compression are reasons to support it later, not reasons to start with it:
  its write path is by far the largest to build.
- **Upstream tools, not our own.** e2fsprogs formats, checks and grows the
  volume; SlopOS writes none of those tools. The journal is jbd2 and seals go
  through the kernel, so SlopOS's format has no extension that e2fsprogs does
  not know.
- **One profile, in one file.** `ext4-core/profile` is what `mke2fs` is asked
  for and what the kernel writes; the installer formats from it too. A
  feature joins it only once the kernel writes it. `ext_attr`, which
  `tune2fs` cannot clear from a converted volume, is read and released but
  never written.
- **Block numbers are 32 bits inside the kernel.** That is 16 TiB at 4 KiB
  blocks; a larger volume is refused at mount. Widening them is a kernel
  change, not a format one: the profile is already `64bit`.
- **At rest is three facts:** `s_state` clean, no `needs_recovery`, and an
  empty journal. The flag reaches the medium before the journal goes live and
  leaves it after the journal empties, so no crash shows a live journal under
  a clear flag. The host's checks ask all three (`scripts/lib/ext4.sh`).
- **A replay is all or nothing.** Every committed copy is checked before any
  is written home; one that fails leaves the whole journal to e2fsck and the
  volume read-only. A mount that may not write, and a journal the volume does
  not flag, replay nothing either: the volume is refused or read-only.
- **A delete is one transaction.** Freeing a file dirties a block bitmap per
  128 MiB group it spans, so the journal bounds the largest file one delete
  can free: about 2 TiB at the 64 MiB a root carries, past the laptop's
  disk. A delete split across transactions through the orphan list lifts
  it.
- **The seal is the immutable flag, and moving it is a capability.** `Seal`
  covers setting and clearing it, because a path-keyed program-identity
  grant is only as good as the seal on the file it names. An append-only
  file another system marked is sealed too: refusing its appends is stricter
  than Linux and never weaker. `TASK_FLAG_SYSTEM`
  confers it today; the installer, which seals the base mount points, gets
  it by program identity in phase 5.
- **Coexistence.** The loader lives in the vendor directory
  `\EFI\SlopOS\` and is reached through a firmware entry; slots live on a
  partition of their own; slot selection is a Boot Loader Interface variable.
  This is how Linux distributions share an ESP, and it keeps a Secure Boot
  chain possible.
- **One statement of the boot disk.** `boot-core` holds the layout, the type
  GUIDs, the GPT reader, the load option and device path codecs, the boot
  manager variables' formats and the Limine configuration renderer. The
  kernel, `bootctl`, the host's disk builder and the installer all read the
  disk through it.
- **SlopOS's partitions carry type GUIDs of its own:** boot
  `0a5d2380-494f-4bb7-9fa1-76e03c03d1ec`, root
  `dc5e4e29-da9a-4e3e-ac44-38b4ea426284`, crash
  `2f690270-a513-45e2-8e3b-75aa8e44177c`. A Linux on the same disk mounts a
  Discoverable Partitions root or XBOOTLDR as its own, and writes its kernels
  into the latter.
- **Limine reads its configuration beside itself and names no default.** It
  tries `<EFI app path>/limine.conf` before any other name, so it finds its own
  ahead of another loader's on a shared ESP. With `default_entry` unset,
  `LoaderEntryDefault` decides, and Limine falls back to its first entry, slot
  a. Slot files are named by the boot partition's GUID. The configuration is
  written at install alone.
- **The loader variables are machine-wide.** `LoaderEntryDefault` and
  `LoaderEntryOneShot` live under the Boot Loader Interface's GUID, which
  systemd-boot and any other Limine on the machine read and write too. A value
  another loader wrote names no SlopOS entry, so SlopOS boots slot a. The
  laptop's CachyOS boots through GRUB, which reads none of them.
- **The removable-media path belongs to an ESP SlopOS created.** Only there
  does SlopOS write `\EFI\BOOT\BOOTX64.EFI`, with a copy of the configuration
  beside it, which is how a firmware that has lost its entries, or never had
  one, still boots the disk.
- **A firmware entry is found by what it starts.** Registering looks for an
  entry naming the same partition GUID and file path, as systemd's `bootctl`
  does, wherever the device path puts them. It rewrites that entry if it
  differs, and otherwise takes the lowest number that is neither a variable
  nor listed in `BootOrder`. The entry goes first in `BootOrder` only when
  asked; otherwise it keeps its place, or joins the end. With no variable
  enumeration, an entry outside `BootOrder` past that number is not seen.
- **A default bootctl cannot resolve stops installs.** Limine also resolves
  `LoaderEntryDefault` by menu path, so a value naming no offered entry
  leaves which slot boots unknown, and `bootctl install` refuses until
  `bootctl set-default` names one. Every slot has an entry of its own name,
  so the slot an entry boots is never in doubt.
- **The kernel holds the boot manager's variables to their formats.**
  `BootEntry` reaches `Boot####`, `BootOrder` and `BootNext`, and reads
  `BootCurrent`. Every write must be non-volatile with boot and runtime
  access, and must be one of: a load option with a well-formed device path
  list, a `BootOrder` that is not empty, a two-byte `BootNext`. Linux's
  efivarfs validates these formats too, for the same reason: firmware parses
  these variables on every boot, often before anything can recover.
- **The installer is a role, and it took the flag word's last bit.**
  `TASK_FLAG_INSTALL` confers `BootEntry`, not init, which writes no firmware
  entry. `0x0040` is retired, so the next flag means widening the word.
- **QEMU roots the host keeps stay disks of their own.** The persistent and
  self-hosting roots are grown, refreshed and read back as image files, so
  they stay `nvme0n1` beside a boot disk with no root partition. `just
  test-install` builds its disk around the tests root, so the A/B loop also
  runs on the one-disk shape.
- **Linux device names,** plus stable `by-partuuid`, `by-uuid` and `by-label`
  links. Probe order is not stable across machines or boots.
- **The install payload is a Limine module** until a USB mass-storage driver
  exists, and the install is offline: no package host.
- **The live ISO is always an install medium.** `just iso` puts the
  `install` module on it whether or not the payload comes, since the
  installer needs Limine for the ESP; the payload alone is the knob, and
  `PAYLOAD=1` builds the release kernel the installed system builds itself
  with. The suite's ISOs carry none but `test-installer`'s. The kernel serves
  beside the medium the kernel and base Limine booted, so an install puts on
  the disk exactly the system it runs. A medium carries the tarball of every
  recipe the base takes programs from and the scripts that build them, and a
  payload's clone every recipe's, so the source of the GPL programs it
  distributes travels with them.
- **e2fsprogs is handed `Mount`, never raised to it, and only in the base.**
  `/sbin`'s six programs keep the raw-device right as far as their spawner
  holds it (a grant's `delegated` flags), so the installer runs them with it
  and the shell, or a script it starts, cannot format a disk through them; no
  copy a root holds gets anything. `AT_SECURE`, which that brings, makes them
  not dumpable, as a setuid program on Linux is; they read their configuration
  and undo directory with a plain `getenv`, so the installer runs them with an
  emptied environment, the configuration named at a path in the sealed base
  that holds none, and no undo file.
- **The raw-device right lifts the block ceiling.** A process holding
  `Mount` may write any disk beneath every filesystem, so the per-process
  `DiskBlocks` ceiling bounds nothing of it, as `CAP_SYS_RESOURCE` overrides a
  Linux quota; the ceiling follows the right at spawn, fork and `execve`. The
  installer copies a gigabyte and more into the root in one process.
- **A base's recipe programs come from a prefix:** the recipes' on the host,
  `/usr/local` in the guest, so a guest builds the base with the e2fsprogs its
  toolchain carries. A recipe of programs alone installs no library, so its
  static archives and headers reach neither another recipe nor the target
  sysroot.
- **An installed table is written the other copy first:** the backup, or
  the primary when the table was read from the backup, each piece behind a
  flush, so a reader finds the old table or the new one whole at every step.
  An erase writes its protective MBR before either copy, and a GPT behind an
  MBR that does not protect it counts as MBR, so an erase cut short leaves a
  disk only erase takes rather than one other systems read as MBR. The first
  MiB of every new partition is zeroed before the table names it, so one an
  install cut short holds no stale volume. A reuse refuses a disk that holds a
  SlopOS root elsewhere. New partitions go into one free region,
  ESP, boot, root and crash in that order on 1 MiB boundaries, the largest
  region unless one is named; a root stops at 16 TiB. A disk SlopOS is already
  on takes a reinstall over its own partitions (reuse), not a second set.
- **The installed root is the host's root, made by the guest.** The same
  directories, the base mount points sealed (the installer's role carries
  `Seal`), `mke2fs` in the profile with the inode tables left for the kernel to
  initialise as each group is first used, as e2fsprogs does off Linux, and a
  64 MiB journal. `/usr/local` follows the host-tree rule (`tree-core`), from
  the manifest `fs_tree.py` makes for the payload, so a newer stick replaces
  exactly what an older one installed; `/src` is seeded once. An installed
  system boots `panic=reboot`, so a tried slot that panics falls back.
- **FAT names as the specification has Windows make them.** An 8.3 name whose
  base and extension are each one case is a short entry alone; any other gets
  long-name entries before an alias, which takes a numeric tail only when the
  name does not fit 8.3 or lost a character, and the short entry is written
  last, behind a flush.
- **The verified image is not a bare-metal root.** The system already belongs
  to the boot slot: kernel and base, read-only. On bare metal the system's
  integrity comes from Limine's BLAKE2B hash on each loaded file under Secure
  Boot, not from a verified root. `fs/assets/ext2.img` remains the suite's
  verity fixture.
- **The crash partition is the kernel's for the boot.** The store holds it
  under a write claim from the drivers phase on, so no table re-read or
  whole-disk write moves the window the panic path writes, and userland
  reaches the records only through `/dev/crash`. An installer cannot
  repartition the disk it booted from, which a mounted root on it already
  forbids.
- **The panic path reads nothing.** What each slot holds is read at boot and
  kept in memory; a record goes to the first empty slot after the newest, else
  over the oldest. A slot being written or erased is busy, so an erase on the
  I/O queue and a panic's write on the panic queue never meet in one slot.
- **A crash store needs a polled queue.** The panic write goes through
  `PanicQueue` with interrupts off and nothing allocated. A disk on any other
  transport keeps no crash record until that transport has a polled panic
  path of its own.
- **A record names its slot by the kernel's own path.** Limine reports the
  path it loaded the kernel from, `/boot/<slot>/kernel.elf`, so a record is
  attributed without the panic or the next boot asking a UEFI variable.
- **A record is erased only once its copy is durable on a disk.** `bootctl
  collect` takes the copy's index from `/var/log/crash/bounds` before writing
  it, writes it beside its final name, flushes it, renames it, flushes the
  directory, and only then unlinks the record; on a RAM root it erases
  nothing. An erase names the record's sequence number as well as its slot,
  so it never removes a newer record that took the slot.
- **A slot keeps two facts: how its last boot ended and its last crash.**
  Under `/var/lib/slopos/slots/<slot>`, `collect` notes `crashed` and the
  copy for each record, then `up` for the slot this boot came from, keeping
  the crash; `bootctl install` clears both, since a new system has not
  booted.
- **Every fatal path that can leave a record does.** The format-free abort a
  lockup takes writes one from its message and resets under `panic=reboot`
  only after it; one a stack overflow takes does not, since writing needs the
  data stack that overflowed. A recovered panic's report goes into the kernel
  log so a later record carries it.
- **`panic=reboot` holds the screen only on bare metal,** for ten seconds,
  the split `watchdog.panic`'s default makes: a hypervisor's log is on the
  host.
- **The `limine` crate misreads `struct limine_file` past `media_type`.**
  Its `File` leaves out the protocol's `unused` word, and its `Uuid` is not
  `repr(C)`, so the GPT disk GUID is rebuilt from where those fields were
  read (`boot/src/limine_protocol.rs`). Any other field after `media_type`
  needs the same treatment until the crate is fixed.
- **Wired NIC first; Wi-Fi is out.** Wi-Fi means a driver per chip, firmware
  blobs, an 802.11 stack and WPA.
- **The stack owns the poll threads; a NIC driver is a `NetDevice`.**
  netpoll and net-timer start at boot whatever NICs there are. A driver
  supplies the device, an interrupt handler that only wakes netpoll,
  `rx_pending` and `sample_carrier`, and calls `nic::publish`, which starts
  DHCP. A USB NIC is the same and nothing more.
- **A frame carries the identity of the device it leaves on.** The source
  MAC is stamped once the route has picked the device, an ARP names that
  device's address, and a neighbour's packets leave on the neighbour's device.
- **A physical NIC delivers only what is addressed to the host.** The weak
  host model, as Linux's default: a datagram for any of the host's addresses
  but a host-scoped one, broadcast and multicast; never 127/8, and no echo
  reply to a broadcast. The one exception is DHCP's: until a device holds an
  address, an unfragmented datagram to the DHCP client port reaches it,
  because a server that ignores the client's broadcast flag unicasts to the
  address it offers.
- **A NIC driver names the versions it brings up.** Each Realtek MAC version
  has a bring-up sequence of its own, so the table holds the RTL8168h and the
  RTL8168M, which is the same MAC, and a new version joins it with its
  sequence when a machine that has one is seen. The PHY patch firmware Linux
  loads for this version is not carried: it is a binary blob, and Linux's
  driver brings the chip up without it when the file is missing.
- **NIC register facts come from drivers, taken as facts.** No public
  Realtek document describes the RTL8168h's bring-up, so its offsets, bit
  values and order come from Linux's `r8169` (GPL-2.0-only) and Redox's
  `rtl8168d` (MIT); code and prose come from neither.
- **A device never writes past the buffer it was given.** The RTL8168's
  receive filter is the length its descriptors name. A larger filter over
  smaller buffers was the precondition of CVE-2009-1389.
- **No device bring-up under a spinlock.** A chip's bring-up busy-waits for
  tens to hundreds of milliseconds, so a driver runs it with its spinlock
  released and the device out of reach, serialised by a sleeping mutex.
- **git reaches another machine over SSH, with OpenSSH's own client.** git
  runs an `ssh` program for an `ssh://` remote, and an unauthenticated `git
  daemon` on a LAN is not the route; OpenSSH's client is the one git is tested
  against. Its whole install lands in the toolchain, as e2fsprogs's does, and
  nothing runs the server or the helpers.

## Constraints

- Everything in `plans/self-hosting.md`'s Constraints applies.
- The installer writes nothing outside the partitions it created or was given,
  and nothing on a shared ESP outside `\EFI\SlopOS\`. The exceptions are UEFI
  variables: the Boot Loader Interface's `LoaderEntryDefault` (and clearing a
  SlopOS `LoaderEntryOneShot`), and the firmware entry, written last.
- No ext4, jbd2, NVMe or NIC driver code or prose from Linux. Format and
  register facts come from the specifications, from kernel.org's layout
  documentation and, for the NIC, from the drivers Decided names.
