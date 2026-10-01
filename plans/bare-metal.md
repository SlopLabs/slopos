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

Nothing persists yet, and nothing reaches the network:

- **Network.** The only NIC driver is virtio-net, and it starts the DHCP
  client itself. `net/src/ipv4.rs` sends a resolved neighbour's queued packets
  through a hard-coded `DevIndex(1)`.
- **Making a root.** The host makes every root: `mke2fs` in the profile, then
  `debugfs` writes the seals (`scripts/build_fs_image.sh`). The guest has no
  mkfs, fsck or resize tool, and nothing that writes a partition table.
- **Boot disk.** The boot disk is a host-built GPT with one ESP. Limine sits
  at the removable-media path, and both slots are on the ESP
  (`scripts/build_bootdisk.sh`). `bootctl` finds the ESP by scanning every
  block node and commits by rewriting `/limine.conf`. Limine is pinned at
  12.3.1.
- **Panics.** `panic=reboot`, which is how a broken slot falls back to the
  committed one, resets at once. On a machine with no COM1 the screen is the
  only record of the panic, and the reset erases it.

## Phases

There are seven phases. Each ends in something that runs, and each lands as
commits of its own.

| Phase | Needs | Ends with |
|---|---|---|
| 1. NVMe disks — **done** | — | the dev loop on NVMe; the live ISO sees the laptop's disk |
| 2. ext4 — **done** | — | every image the tree builds is ext4 |
| 3. A boot chain that shares a disk | 1 | the A/B loop on the new partition layout |
| 4. A crash record | 1, 3 | a slot that panics leaves the panic behind |
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

### Phase 3: A boot chain that shares a disk

Every SlopOS disk has this layout, the QEMU boot disk included
(`scripts/build_bootdisk.sh` builds it):

| Partition | Filesystem and type | Contents | Written by |
|---|---|---|---|
| ESP | the disk's existing ESP, or a new one if there is none | `\EFI\SlopOS\BOOTX64.EFI` (Limine) and `\EFI\SlopOS\limine.conf` | installer |
| SlopOS boot | FAT32, SlopOS type GUID, 1 GiB | `/boot/{a,b}/kernel.elf` and `base.img` | `bootctl` |
| SlopOS root | ext4, SlopOS type GUID | `/` | the system |
| SlopOS crash | raw, SlopOS type GUID, 4 MiB | the last panic | the panic path |

The boot partition is 1 GiB because the largest slot, the tests kernel (97 MB)
with the tests base (64 MB), is 161 MB, and there are two slots.

- **Limine 12.9 or later.** Limine reads `<EFI app path>/limine.conf` first,
  so it coexists with another Limine or with GRUB on the same ESP. It
  publishes `LoaderDevicePartUUID`. Since 12.7 it publishes `LoaderEntries`
  and resolves `LoaderEntryDefault`, and since 12.9 it consults
  `LoaderEntryDefault` only while `default_entry` is unset. Kernel and base
  paths are `guid(<partuuid>):/boot/<slot>/…`.
- **bootctl.** `bootctl` looks for the boot partition, by its type GUID, on
  the disk named by `LoaderDevicePartUUID`. It commits by writing
  `LoaderEntryDefault`, a Boot Loader Interface variable that is already
  allowed, instead of rewriting `limine.conf`. `limine.conf` then changes only
  at install. Secure Boot needs that later: Limine's enrolled config hash has
  to survive every commit.
- **Firmware entry.** The firmware learns about the loader through a
  `Boot####` entry for `\EFI\SlopOS\BOOTX64.EFI`, placed first in `BootOrder`
  only if the user asks. Those variables live under the EFI global GUID,
  which `core/src/efivar.rs` refuses today, rightly. A new `BootEntry`
  capability grants `Boot####`, `BootOrder` and `BootNext` and nothing else,
  and only the installer gets it.
- **Fallback path.** `\EFI\BOOT\BOOTX64.EFI` is written only on an ESP the
  installer created. On a shared ESP, the removable-media path belongs to
  whoever put a loader there.
- **Other OSes.** The Limine menu carries an `efi_boot_entry` entry for each
  other OS's firmware entry, so SlopOS's menu can boot CachyOS. The other
  direction goes through the firmware's boot menu: GRUB's os-prober finds no
  Linux kernel on SlopOS's partitions.

**Done when** the QEMU boot disk carries the new layout under Limine 12.9 or
later, and `just test-install` commits and rolls back through
`LoaderEntryDefault` with `limine.conf` untouched.

### Phase 4: A crash record

- **Write.** The panic path writes the panic report and the tail of the
  kernel log to the crash partition, through NVMe's reserved polled queue: no
  wait, no allocation, no interrupt. Then it flushes, then it resets.
- **Read back.** On the next boot, a service moves the record to
  `/var/log/crash/` and clears the partition. `bootctl status` reports that
  the slot's last boot crashed.
- **Why not RAM:** whether memory survives a reset is up to the firmware
  (memory training, and TME re-keying).
- **Why not a UEFI variable:** the panic path cannot reach runtime services
  from a user task's address space (`boot/src/shutdown.rs`), and NVRAM is
  small and wears out.
- **Screen hold.** On bare metal, `panic=reboot` holds the screen for a few
  seconds before resetting; under a hypervisor it does not wait. This is the
  same split `watchdog.panic`'s default already makes. A panic before the
  root disk is probed leaves only the screen.

**Done when** a slot booted with `panic.boot=on` in QEMU falls back to the
committed slot, and the fallback boot finds the panic in `/var/log/crash/`.

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
   profile for the root.
5. Seal the base mount points.
6. Copy the payload, and record a manifest under `/var/lib/slopos/trees`. The
   rule is the one host trees follow, so a newer stick updates `/usr/local`
   without touching what the user added.
7. Write both slots, and `limine.conf` with `cmdline: root=PARTUUID=…` and no
   QEMU `resolution:` line.
8. Set `LoaderEntryDefault`.
9. Run `e2fsck -fn` on the new root.
10. Register the firmware entry, last.

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
  byte-identical, and the foreign firmware entry is still in `BootOrder`.
- **An existing partition,** reused as the root.

Each run then boots from the disk with the ISO detached, and goes around
`selfhost.sh install` once. The host holds the root to `e2fsck -fn`.

**Crash record.** A slot booted with `panic.boot=on` falls back to the
committed slot. The fallback boot finds the panic in `/var/log/crash/`.

**Filesystem.** `just check-fs-image` and `just test-rude-exit` run in CI.

## Out of scope

- **Wi-Fi.**
- **USB.** See `plans/usb-xhci.md`. It is what later lets the live system read
  the medium from the stick instead of from RAM, and what brings USB NICs.
- **Other machines' platforms:** AHCI (the laptop's SATA controller has no
  disk), VMD, more than 17 CPUs, timers without HPET, x2APIC mode, INTx, PCI
  without MCFG, other PCH GPIO blocks.
- **ACPI events and suspend.**
- **Secure Boot signing.** The design allows it (commits do not touch
  `limine.conf`); the signing itself is not planned.
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
- **Linux device names,** plus stable `by-partuuid`, `by-uuid` and `by-label`
  links. Probe order is not stable across machines or boots.
- **The install payload is a Limine module** until a USB mass-storage driver
  exists, and the install is offline: no package host.
- **The verified image is not a bare-metal root.** The system already belongs
  to the boot slot: kernel and base, read-only. On bare metal the system's
  integrity comes from Limine's BLAKE2B hash on each loaded file under Secure
  Boot, not from a verified root. `fs/assets/ext2.img` remains the suite's
  verity fixture.
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
