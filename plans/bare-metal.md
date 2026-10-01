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

Nothing reaches the network, and the guest cannot make a disk:

- **Network.** The only NIC driver is virtio-net, and it starts the DHCP
  client itself. `net/src/ipv4.rs` sends a resolved neighbour's queued packets
  through a hard-coded `DevIndex(1)`.
- **Making a disk.** The host makes every root and every boot disk: `mke2fs`
  in the profile, then `debugfs` writes the seals
  (`scripts/build_fs_image.sh`); `sfdisk`, `mkfs.fat` and mtools lay out the
  boot disk (`scripts/build_bootdisk.sh`). The guest has no mkfs, fsck or
  resize tool, nothing that writes a partition table, and `fat-core` creates
  8.3 names only, so it cannot write `\EFI\SlopOS\limine.conf`.

## Phases

There are seven phases. Each ends in something that runs, and each lands as
commits of its own.

| Phase | Needs | Ends with |
|---|---|---|
| 1. NVMe disks — **done** | — | the dev loop on NVMe; the live ISO sees the laptop's disk |
| 2. ext4 — **done** | — | every image the tree builds is ext4 |
| 3. A boot chain that shares a disk — **done** | 1 | the A/B loop on the new partition layout |
| 4. A crash record — **done** | 1, 3 | a slot that panics leaves the panic behind |
| 5. Installer and install medium | 1, 2, 3 | **milestone 1:** SlopOS installed beside CachyOS, self-hosting offline |
| 6. Wired network | — | **milestone 2:** git and crates.io over the RJ45 port |
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

### Phase 5: Installer and install medium (milestone 1)

**The medium** is the ISO, flashed to a USB stick.

- Limine loads the kernel and the live base into RAM.
- When the ISO is built with the payload knob on `just iso`, Limine also loads
  a second module, `install`. It is a newc archive of:
  - `/usr/local`: the toolchain, 835 MB here;
  - `/src/slopos`: a `--vendored` clone, with the llvm-project tarball.
- The kernel serves the payload read-only at `/media/install`, in place where
  Limine put it, as a second instance of `fs/src/basefs.rs`.
- A Limine module is the only way in. The kernel has no USB driver, so once it
  runs it cannot read the stick. This is the `copytoram` route of Linux live
  ISOs.
- An ISO built without the payload installs the system alone.

**e2fsprogs** becomes a recipe and ships in the base: `mke2fs`, `e2fsck`,
`resize2fs`, `tune2fs`, `debugfs` and `dumpe2fs`.

- The installer formats with it.
- A damaged root is repaired from the stick.
- The guest's base build takes e2fsprogs from `/usr/local`, as it takes clang
  from there. Rebuilding recipes in the guest is Phase 2 of
  `plans/self-hosting.md`.

**The installer** is a text program run from the live system's terminal.
Every answer is also a flag, so the QEMU check drives the same program a
person runs. It works in this order:

1. The user picks a disk and a mode:
   - erase the whole disk;
   - install into a free region;
   - reuse an existing partition the user names.

   Nothing else on the disk is written. Shrinking another OS's filesystem is
   done from that OS.
2. Write the GPT: partitions aligned to 1 MiB, fresh GUIDs from `getrandom`,
   primary and backup tables written together.
3. Re-read the partition table.
4. Format the new partitions: `fat-core` for FAT32, `mke2fs` with the feature
   profile for the root. `fat-core` learns to create long names first:
   `\EFI\SlopOS` is mixed case and `limine.conf` is no 8.3 name.
5. Seal the base mount points.
6. Copy the payload, and record a manifest under `/var/lib/slopos/trees`. The
   rule is the one host trees follow, so a newer stick updates `/usr/local`
   without touching what the user added.
7. Write both slots, and `limine.conf` from `boot_core::limine`: `cmdline:
   root=PARTUUID=…`, no QEMU `resolution:` line, a timeout that shows the
   menu, and an `efi_boot_entry` for each option `BootOrder` lists that
   `limine::offered_title` accepts given all of them, leaving out one that
   `limine::collide`s with an entry already in the menu. It
   goes to `\EFI\SlopOS\`, and on an ESP the installer created to the
   removable-media path as well.
8. Set `LoaderEntryDefault`.
9. Run `e2fsck -fn` on the new root.
10. Register the firmware entry, last (`boot_disk::register_firmware_entry`),
    first in `BootOrder` only if the user asks.

The installer holds `Mount`, `Power` and the installer's role,
`TASK_FLAG_INSTALL`, by program identity; the role confers `BootEntry`, and
`Seal` joins it here.

The clone's remotes are set to whatever the user names: GitHub over HTTPS, or
the development machine. None point at SLIRP.

**Done when** `just test-installer` passes its three disks, and on the laptop
SlopOS installs beside CachyOS from the stick, boots from the NV3, and runs
`selfhost.sh install`, `bootctl reboot` and `bootctl commit` with no network.

### Phase 6: Wired network (milestone 2)

- **Driver.** A driver for the laptop's Realtek RTL8111/8168 (`10ec:8168`).
  The chip reports its exact MAC version in its TxConfig register at probe.
  The driver carries a table of the versions it knows, and declines any
  other.
- **Sources.** Register and descriptor facts come from Realtek's published
  RTL8169/8111-family datasheets and from Redox's `rtl8168d`, which is MIT.
  Linux's `r8169` and Realtek's `r8168` are GPL-2.0-only, and FreeBSD's
  `re(4)` is BSD-4-Clause; from those, facts only, never code. The PHY patch
  file Linux loads for some chip versions is not carried.
- **Grading.** QEMU has no model of this chip, so the driver is the one piece
  of this plan the suite cannot boot. Its descriptor rings and version table
  live in a host-tested core crate, as `tls-core` does, so `just test-host`
  grades everything but the hardware. The laptop grades the rest.
- **DHCP.** DHCP starts for every NIC that registers, not just virtio-net.
  `ipv4.rs` sends on the device the neighbour belongs to.
- **Remotes.** HTTPS to GitHub already works; `just test-toolchain` grades it.
  To reach the development machine over the LAN, git needs an `ssh` program
  (git runs one; nothing in the tree provides it), so an OpenSSH client
  becomes a recipe. An unauthenticated `git daemon` on a LAN is not the route.

**Done when** `just test-host` grades the driver's core crate, and on the
laptop the RJ45 port takes a DHCP lease, `git pull` and `git push` reach the
development machine over SSH, and cargo fetches from crates.io.

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
  entry. Afterwards the foreign partition and the foreign ESP files are
  byte-identical, and the foreign firmware entry is still in `BootOrder`. The
  foreign entry names a partition on the disk QEMU boots by `bootindex`: OVMF
  deletes an `HD()` entry it cannot match to such a device.
- **An existing partition,** reused as the root.

Each run then boots from the disk with the ISO detached, and goes around
`selfhost.sh install` once. The host holds the root to `e2fsck -fn`.

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

## Constraints

- Everything in `plans/self-hosting.md`'s Constraints applies.
- The installer writes nothing outside the partitions it created or was given,
  and nothing on a shared ESP outside `\EFI\SlopOS\`. The one exception is the
  firmware entry, written last.
- No ext4, jbd2, NVMe or NIC driver code or prose from Linux. Format and
  register facts come from the specifications and from kernel.org's layout
  documentation.
